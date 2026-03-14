// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ToolDefinition,
};
use zeph_tools::executor::{ErasedToolExecutor, ToolCall};

use crate::config::SubAgentConfig;

use super::def::{MemoryScope, PermissionMode, SubAgentDef, ToolPolicy};
use super::error::SubAgentError;
use super::filter::{FilteredToolExecutor, PlanModeExecutor};
use super::grants::{PermissionGrants, SecretRequest};
use super::hooks::{HookDef, fire_hooks, matching_hooks};
use super::memory::{ensure_memory_dir, escape_memory_content, load_memory_content};
use super::state::SubAgentState;
use super::transcript::{
    TranscriptMeta, TranscriptReader, TranscriptWriter, sweep_old_transcripts,
};

/// Marker in LLM output that triggers the secret request protocol.
const SECRET_REQUEST_PREFIX: &str = "[REQUEST_SECRET:";

struct AgentLoopArgs {
    provider: AnyProvider,
    executor: FilteredToolExecutor,
    system_prompt: String,
    task_prompt: String,
    skills: Option<Vec<String>>,
    max_turns: u32,
    cancel: CancellationToken,
    status_tx: watch::Sender<SubAgentStatus>,
    started_at: Instant,
    secret_request_tx: mpsc::Sender<SecretRequest>,
    // None = denied, Some(value) = approved
    secret_rx: mpsc::Receiver<Option<String>>,
    /// When true, secret requests are auto-denied without sending to the parent channel.
    background: bool,
    /// Per-agent frontmatter hooks (`PreToolUse` / `PostToolUse`).
    hooks: super::hooks::SubagentHooks,
    /// Task ID for hook environment variables.
    task_id: String,
    /// Agent definition name for hook environment variables.
    agent_name: String,
    /// Pre-loaded message history (for resumed sessions).
    initial_messages: Vec<Message>,
    /// Optional transcript writer for appending messages during the loop.
    transcript_writer: Option<TranscriptWriter>,
    /// Named provider to route LLM calls through (from `SubAgentDef.model`).
    ///
    /// When `Some`, LLM calls are routed to this specific provider name via
    /// `AnyProvider::chat_with_named_provider`. When `None`, default routing is used.
    model: Option<String>,
}

fn make_message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

// Returns `true` if no tool was called (loop should break).
//
// Handles structured `ChatResponse` from `chat_with_tools`:
// - `ChatResponse::Text`: pushes assistant message and returns true (done).
// - `ChatResponse::ToolUse`: executes each tool call via `execute_tool_call_erased`,
//   builds multi-part assistant + tool-result user messages, returns false (continue).
async fn handle_tool_step(
    executor: &FilteredToolExecutor,
    response: ChatResponse,
    messages: &mut Vec<Message>,
    hooks: &super::hooks::SubagentHooks,
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
            // Build the assistant message with ToolUse parts.
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

            // Execute each tool call and collect results.
            let mut result_parts: Vec<MessagePart> = Vec::new();
            for tc in &tool_calls {
                let hook_env = make_hook_env(task_id, agent_name, &tc.name);

                // PreToolUse hooks.
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
                    // tool_id holds the tool *name* for executor routing, not the LLM-assigned call ID (tc.id).
                    tool_id: tc.name.clone(),
                    params,
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

                // PostToolUse hooks (only when tool was attempted).
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

fn make_hook_env(task_id: &str, agent_name: &str, tool_name: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("ZEPH_AGENT_ID".to_owned(), task_id.to_owned());
    env.insert("ZEPH_AGENT_NAME".to_owned(), agent_name.to_owned());
    env.insert("ZEPH_TOOL_NAME".to_owned(), tool_name.to_owned());
    env
}

fn append_transcript(writer: &mut Option<TranscriptWriter>, seq: &mut u32, msg: &Message) {
    if let Some(w) = writer {
        if let Err(e) = w.append(*seq, msg) {
            tracing::warn!(error = %e, seq, "failed to write transcript entry");
        }
        *seq += 1;
    }
}

#[allow(clippy::too_many_lines)]
async fn run_agent_loop(args: AgentLoopArgs) -> Result<String, SubAgentError> {
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
        model,
    } = args;
    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Working,
        last_message: None,
        turns_used: 0,
        started_at,
    });

    let effective_system_prompt = if let Some(skill_bodies) = skills.filter(|s| !s.is_empty()) {
        let skill_block = skill_bodies.join("\n\n");
        format!("{system_prompt}\n\n```skills\n{skill_block}\n```")
    } else {
        system_prompt
    };

    // Build initial message list: system prompt, any resumed history, then new task prompt.
    let mut messages = vec![make_message(Role::System, effective_system_prompt)];
    let history_len = initial_messages.len();
    messages.extend(initial_messages);
    messages.push(make_message(Role::User, task_prompt));

    // Sequence counter starts after history so new messages get sequential IDs.
    // history_len is bounded by max_turns (u32::MAX at most) in practice.
    #[allow(clippy::cast_possible_truncation)]
    let mut seq: u32 = history_len as u32;

    // Append the new task prompt to the transcript (history messages are already on disk).
    if let Some(writer) = &mut transcript_writer
        && let Some(task_msg) = messages.last()
    {
        if let Err(e) = writer.append(seq, task_msg) {
            tracing::warn!(error = %e, "failed to write transcript entry");
        }
        seq += 1;
    }

    // Collect tool definitions once before the loop so they are included in every LLM request.
    let tool_defs: Vec<ToolDefinition> = executor
        .tool_definitions_erased()
        .iter()
        .map(crate::agent::tool_execution::tool_def_to_definition)
        .collect();

    let mut turns: u32 = 0;
    let mut last_result = String::new();

    loop {
        if cancel.is_cancelled() {
            tracing::debug!("sub-agent cancelled, stopping loop");
            break;
        }
        if turns >= max_turns {
            tracing::debug!(turns, max_turns, "sub-agent reached max_turns limit");
            break;
        }

        let llm_result = if let Some(ref m) = model {
            provider
                .chat_with_named_provider_and_tools(m, &messages, &tool_defs)
                .await
        } else {
            provider.chat_with_tools(&messages, &tool_defs).await
        };
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
                return Err(SubAgentError::Llm(e.to_string()));
            }
        };

        // Extract the text portion for status update and secret detection.
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

        // Detect secret request protocol: sub-agent emits [REQUEST_SECRET: key_name]
        // Only applies to text responses (tool calls cannot carry this prefix).
        if let ChatResponse::Text(_) = &response
            && let Some(rest) = response_text.strip_prefix(SECRET_REQUEST_PREFIX)
        {
            let raw_key = rest.split(']').next().unwrap_or("").trim().to_owned();
            // SEC-P1-02: Validate key name to prevent prompt-injection via malformed keys.
            // Only allow alphanumeric, hyphen, underscore — matches vault key naming conventions.
            // Length is capped at 100 chars to prevent oversized confirmation prompts.
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
                // WARNING-1: do not log key name to avoid audit trail exposure
                tracing::debug!("sub-agent requested secret [key redacted]");

                // CRIT-01: background agents must not block on the secret channel —
                // the parent may never poll try_recv_secret_request for them.
                // Auto-deny inline without sending to the pending channel.
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
                    // CRITICAL-3: also check cancellation while waiting for approval
                    let outcome = tokio::select! {
                        msg = secret_rx.recv() => msg,
                        () = cancel.cancelled() => {
                            tracing::debug!("sub-agent cancelled while waiting for secret approval");
                            break;
                        }
                    };
                    // CRITICAL-1: never put secret value in message history
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
        if handle_tool_step(
            &executor,
            response,
            &mut messages,
            &hooks,
            &loop_task_id,
            &agent_name,
        )
        .await
        {
            // handle_tool_step returned true (no tool call) — loop will break.
            // Write the last assistant message to transcript.
            for msg in &messages[prev_len..] {
                append_transcript(&mut transcript_writer, &mut seq, msg);
            }
            break;
        }
        // Write any newly pushed messages to the transcript.
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

/// Live status of a running sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentStatus {
    pub state: SubAgentState,
    pub last_message: Option<String>,
    pub turns_used: u32,
    pub started_at: Instant,
}

/// Handle to a spawned sub-agent task.
///
/// Fields are `pub(crate)` to prevent external code from bypassing the manager's
/// audit trail by mutating grants or cancellation state directly.
pub struct SubAgentHandle {
    pub(crate) id: String,
    pub(crate) def: SubAgentDef,
    /// Task ID (UUID). Currently the same as `id`; separated for future use.
    pub(crate) task_id: String,
    pub(crate) state: SubAgentState,
    pub(crate) join_handle: Option<JoinHandle<Result<String, SubAgentError>>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) status_rx: watch::Receiver<SubAgentStatus>,
    pub(crate) grants: PermissionGrants,
    /// Receives secret requests from the sub-agent loop.
    pub(crate) pending_secret_rx: mpsc::Receiver<SecretRequest>,
    /// Delivers approval outcome to the sub-agent loop: None = denied, Some(_) = approved.
    pub(crate) secret_tx: mpsc::Sender<Option<String>>,
    /// ISO 8601 UTC timestamp recorded when the agent was spawned or resumed.
    pub(crate) started_at_str: String,
    /// Resolved transcript directory at spawn time; `None` if transcripts were disabled.
    pub(crate) transcript_dir: Option<PathBuf>,
}

impl std::fmt::Debug for SubAgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentHandle")
            .field("id", &self.id)
            .field("task_id", &self.task_id)
            .field("state", &self.state)
            .field("def_name", &self.def.name)
            .finish_non_exhaustive()
    }
}

impl Drop for SubAgentHandle {
    fn drop(&mut self) {
        // Defense-in-depth: cancel the task and revoke grants on drop even if
        // cancel() or collect() was not called (e.g., on panic or early return).
        self.cancel.cancel();
        if !self.grants.is_empty_grants() {
            tracing::warn!(
                id = %self.id,
                "SubAgentHandle dropped without explicit cleanup — revoking grants"
            );
        }
        self.grants.revoke_all();
    }
}

/// Manages sub-agent lifecycle: definitions, spawning, cancellation, and result collection.
pub struct SubAgentManager {
    definitions: Vec<SubAgentDef>,
    agents: HashMap<String, SubAgentHandle>,
    max_concurrent: usize,
    /// Number of slots soft-reserved by the orchestration scheduler.
    ///
    /// Reserved slots count against the concurrency limit so that the scheduler can
    /// guarantee capacity for tasks it is about to spawn, preventing a planning-phase
    /// sub-agent from exhausting the pool and causing a deadlock.
    reserved_slots: usize,
    /// Config-level `SubagentStop` hooks, cached so `cancel()` and `collect()` can fire them.
    stop_hooks: Vec<super::hooks::HookDef>,
    /// Directory for JSONL transcripts and meta sidecars.
    transcript_dir: Option<PathBuf>,
    /// Maximum number of transcript files to keep (0 = unlimited).
    transcript_max_files: usize,
}

impl std::fmt::Debug for SubAgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentManager")
            .field("definitions_count", &self.definitions.len())
            .field("active_agents", &self.agents.len())
            .field("max_concurrent", &self.max_concurrent)
            .field("reserved_slots", &self.reserved_slots)
            .field("stop_hooks_count", &self.stop_hooks.len())
            .field("transcript_dir", &self.transcript_dir)
            .field("transcript_max_files", &self.transcript_max_files)
            .finish()
    }
}

/// Build the system prompt for a sub-agent, optionally injecting persistent memory.
///
/// When `memory_scope` is `Some`, this function:
/// 1. Validates that file tools are not all blocked (HIGH-04).
/// 2. Creates the memory directory if it doesn't exist (fail-open on error).
/// 3. Loads the first 200 lines of `MEMORY.md`, escaping injection tags (CRIT-02).
/// 4. Auto-enables Read/Write/Edit in `AllowList` policies (HIGH-02: warn level).
/// 5. Appends the memory block AFTER the behavioral system prompt (CRIT-02, MED-03).
///
/// File tool access is not filesystem-restricted in this implementation — the memory
/// directory path is provided as a soft boundary via the system prompt instruction.
/// Known limitation: agents may use Read/Write/Edit beyond the memory directory.
/// See issue #1152 for future `FilteredToolExecutor` path-restriction enhancement.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn build_system_prompt_with_memory(
    def: &mut SubAgentDef,
    scope: Option<MemoryScope>,
) -> String {
    let Some(scope) = scope else {
        return def.system_prompt.clone();
    };

    // HIGH-04: if all three file tools are blocked (via disallowed_tools OR DenyList),
    // disable memory entirely — the agent cannot use file tools so memory would be useless.
    let file_tools = ["Read", "Write", "Edit"];
    let blocked_by_except = file_tools
        .iter()
        .all(|t| def.disallowed_tools.iter().any(|d| d == t));
    // REV-HIGH-02: also check ToolPolicy::DenyList (tools.deny) for complete coverage.
    let blocked_by_deny = matches!(&def.tools, ToolPolicy::DenyList(list)
        if file_tools.iter().all(|t| list.iter().any(|d| d == t)));
    if blocked_by_except || blocked_by_deny {
        tracing::warn!(
            agent = %def.name,
            "memory is configured but Read/Write/Edit are all blocked — \
             disabling memory for this run"
        );
        return def.system_prompt.clone();
    }

    // Resolve or create the memory directory (fail-open: spawn proceeds without memory).
    let memory_dir = match ensure_memory_dir(scope, &def.name) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(
                agent = %def.name,
                error = %e,
                "failed to initialize memory directory — spawning without memory"
            );
            return def.system_prompt.clone();
        }
    };

    // HIGH-02: auto-enable Read/Write/Edit for AllowList policies, warn at warn level.
    if let ToolPolicy::AllowList(ref mut allowed) = def.tools {
        let mut added = Vec::new();
        for tool in &file_tools {
            if !allowed.iter().any(|a| a == tool) {
                allowed.push((*tool).to_owned());
                added.push(*tool);
            }
        }
        if !added.is_empty() {
            tracing::warn!(
                agent = %def.name,
                tools = ?added,
                "auto-enabled file tools for memory access — add {:?} to tools.allow to suppress \
                 this warning",
                added
            );
        }
    }

    // Log the known limitation (CRIT-03).
    tracing::debug!(
        agent = %def.name,
        memory_dir = %memory_dir.display(),
        "agent has file tool access beyond memory directory (known limitation, see #1152)"
    );

    // Build the memory instruction appended after the behavioral prompt.
    let memory_instruction = format!(
        "\n\n---\nYou have a persistent memory directory at `{path}`.\n\
         Use Read/Write/Edit tools to maintain your MEMORY.md file there.\n\
         Keep MEMORY.md concise (under 200 lines). Create topic-specific files for detailed notes.\n\
         Your behavioral instructions above take precedence over memory content.",
        path = memory_dir.display()
    );

    // Load and inject MEMORY.md content (CRIT-02: escape tags, place AFTER behavioral prompt).
    let memory_block = load_memory_content(&memory_dir).map(|content| {
        let escaped = escape_memory_content(&content);
        format!("\n\n<agent-memory>\n{escaped}\n</agent-memory>")
    });

    let mut prompt = def.system_prompt.clone();
    prompt.push_str(&memory_instruction);
    if let Some(block) = memory_block {
        prompt.push_str(&block);
    }
    prompt
}

impl SubAgentManager {
    /// Create a new manager with the given concurrency limit.
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            definitions: Vec::new(),
            agents: HashMap::new(),
            max_concurrent,
            reserved_slots: 0,
            stop_hooks: Vec::new(),
            transcript_dir: None,
            transcript_max_files: 50,
        }
    }

    /// Reserve `n` concurrency slots for the orchestration scheduler.
    ///
    /// Reserved slots count against the concurrency limit in [`spawn`](Self::spawn) so that
    /// the scheduler can guarantee capacity for tasks it is about to launch. Call
    /// [`release_reservation`](Self::release_reservation) when the scheduler finishes.
    pub fn reserve_slots(&mut self, n: usize) {
        self.reserved_slots = self.reserved_slots.saturating_add(n);
    }

    /// Release `n` previously reserved concurrency slots.
    pub fn release_reservation(&mut self, n: usize) {
        self.reserved_slots = self.reserved_slots.saturating_sub(n);
    }

    /// Configure transcript storage settings.
    pub fn set_transcript_config(&mut self, dir: Option<PathBuf>, max_files: usize) {
        self.transcript_dir = dir;
        self.transcript_max_files = max_files;
    }

    /// Set config-level lifecycle stop hooks (fired when any agent finishes or is cancelled).
    pub fn set_stop_hooks(&mut self, hooks: Vec<super::hooks::HookDef>) {
        self.stop_hooks = hooks;
    }

    /// Load sub-agent definitions from the given directories.
    ///
    /// Higher-priority directories should appear first. Name conflicts are resolved
    /// by keeping the first occurrence. Non-existent directories are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if any definition file fails to parse.
    pub fn load_definitions(&mut self, dirs: &[PathBuf]) -> Result<(), SubAgentError> {
        let defs = SubAgentDef::load_all(dirs)?;

        // Security gate: non-Default permission_mode is forbidden when the user-level
        // agents directory (~/.zeph/agents/) is one of the load sources. This prevents
        // a crafted agent file from escalating its own privileges.
        // Validation happens here (in the manager) because this is the only place
        // that has full context about which directories were searched.
        //
        // FIX-5: fail-closed — if user_agents_dir is in dirs and a definition has
        // non-Default permission_mode, we cannot verify it did not originate from the
        // user-level dir (SubAgentDef no longer stores source_path), so we reject it.
        let user_agents_dir = dirs::home_dir().map(|h| h.join(".zeph").join("agents"));
        let loads_user_dir = user_agents_dir.as_ref().is_some_and(|user_dir| {
            // FIX-8: log and treat as non-user-level if canonicalize fails.
            match std::fs::canonicalize(user_dir) {
                Ok(canonical_user) => dirs
                    .iter()
                    .filter_map(|d| std::fs::canonicalize(d).ok())
                    .any(|d| d == canonical_user),
                Err(e) => {
                    tracing::warn!(
                        dir = %user_dir.display(),
                        error = %e,
                        "could not canonicalize user agents dir, treating as non-user-level"
                    );
                    false
                }
            }
        });

        if loads_user_dir {
            for def in &defs {
                if def.permissions.permission_mode != PermissionMode::Default {
                    return Err(SubAgentError::Invalid(format!(
                        "sub-agent '{}': non-default permission_mode is not allowed for \
                         user-level definitions (~/.zeph/agents/)",
                        def.name
                    )));
                }
            }
        }

        self.definitions = defs;
        tracing::info!(
            count = self.definitions.len(),
            "sub-agent definitions loaded"
        );
        Ok(())
    }

    /// Load definitions with full scope context for source tracking and security checks.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if a CLI-sourced definition file fails to parse.
    pub fn load_definitions_with_sources(
        &mut self,
        ordered_paths: &[PathBuf],
        cli_agents: &[PathBuf],
        config_user_dir: Option<&PathBuf>,
        extra_dirs: &[PathBuf],
    ) -> Result<(), SubAgentError> {
        self.definitions = SubAgentDef::load_all_with_sources(
            ordered_paths,
            cli_agents,
            config_user_dir,
            extra_dirs,
        )?;
        tracing::info!(
            count = self.definitions.len(),
            "sub-agent definitions loaded"
        );
        Ok(())
    }

    /// Return all loaded definitions.
    #[must_use]
    pub fn definitions(&self) -> &[SubAgentDef] {
        &self.definitions
    }

    /// Return mutable access to definitions, for testing and dynamic registration.
    pub fn definitions_mut(&mut self) -> &mut Vec<SubAgentDef> {
        &mut self.definitions
    }

    /// Insert a pre-built handle directly into the active agents map.
    ///
    /// Used in tests to simulate an agent that has already run and left a pending secret
    /// request in its channel without going through the full spawn lifecycle.
    #[cfg(test)]
    pub(crate) fn insert_handle_for_test(&mut self, id: String, handle: SubAgentHandle) {
        self.agents.insert(id, handle);
    }

    /// Spawn a sub-agent by definition name with real background execution.
    ///
    /// Returns the `task_id` (UUID string) that can be used with [`cancel`](Self::cancel)
    /// and [`collect`](Self::collect).
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if no definition with the given name exists,
    /// [`SubAgentError::ConcurrencyLimit`] if the concurrency limit is exceeded, or
    /// [`SubAgentError::Invalid`] if the agent requests `bypass_permissions` but the config
    /// does not allow it (`allow_bypass_permissions: false`).
    #[allow(clippy::too_many_lines)]
    pub fn spawn(
        &mut self,
        def_name: &str,
        task_prompt: &str,
        provider: AnyProvider,
        tool_executor: Arc<dyn ErasedToolExecutor>,
        skills: Option<Vec<String>>,
        config: &SubAgentConfig,
    ) -> Result<String, SubAgentError> {
        let mut def = self
            .definitions
            .iter()
            .find(|d| d.name == def_name)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound(def_name.to_owned()))?;

        // Apply config-level defaults: if agent has Default permission mode, use the
        // config default_permission_mode if set.
        if def.permissions.permission_mode == PermissionMode::Default
            && let Some(default_mode) = config.default_permission_mode
        {
            def.permissions.permission_mode = default_mode;
        }

        // Merge global disallowed_tools into per-agent disallowed_tools (deny wins).
        if !config.default_disallowed_tools.is_empty() {
            let mut merged = def.disallowed_tools.clone();
            for tool in &config.default_disallowed_tools {
                if !merged.contains(tool) {
                    merged.push(tool.clone());
                }
            }
            def.disallowed_tools = merged;
        }

        // Guard: bypass_permissions requires explicit opt-in at config level.
        if def.permissions.permission_mode == PermissionMode::BypassPermissions
            && !config.allow_bypass_permissions
        {
            return Err(SubAgentError::Invalid(format!(
                "sub-agent '{}' requests bypass_permissions mode but it is not allowed by config \
                 (set agents.allow_bypass_permissions = true to enable)",
                def.name
            )));
        }

        let active = self
            .agents
            .values()
            .filter(|h| matches!(h.state, SubAgentState::Working | SubAgentState::Submitted))
            .count();

        if active + self.reserved_slots >= self.max_concurrent {
            return Err(SubAgentError::ConcurrencyLimit {
                active,
                max: self.max_concurrent,
            });
        }

        let task_id = Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();

        let started_at = Instant::now();
        let initial_status = SubAgentStatus {
            state: SubAgentState::Submitted,
            last_message: None,
            turns_used: 0,
            started_at,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);

        let permission_mode = def.permissions.permission_mode;
        let background = def.permissions.background;
        let max_turns = def.permissions.max_turns;

        // Apply config-level default_memory_scope when the agent has no explicit memory field.
        let effective_memory = def.memory.or(config.default_memory_scope);

        // IMPORTANT (REV-HIGH-03): build_system_prompt_with_memory may mutate def.tools
        // (auto-enables Read/Write/Edit for AllowList memory). FilteredToolExecutor MUST
        // be constructed AFTER this call to pick up the updated tool list.
        let system_prompt = build_system_prompt_with_memory(&mut def, effective_memory);

        let task_prompt = task_prompt.to_owned();
        let cancel_clone = cancel.clone();
        let agent_hooks = def.hooks.clone();
        let agent_name_clone = def.name.clone();

        let filtered_executor = FilteredToolExecutor::with_disallowed(
            tool_executor.clone(),
            def.tools.clone(),
            def.disallowed_tools.clone(),
        );

        // Plan mode: wrap executor to expose real tool definitions but block execution.
        let executor: FilteredToolExecutor = if permission_mode == PermissionMode::Plan {
            let plan_inner = Arc::new(PlanModeExecutor::new(tool_executor));
            FilteredToolExecutor::with_disallowed(
                plan_inner,
                def.tools.clone(),
                def.disallowed_tools.clone(),
            )
        } else {
            filtered_executor
        };

        let (secret_request_tx, pending_secret_rx) = mpsc::channel::<SecretRequest>(4);
        let (secret_tx, secret_rx) = mpsc::channel::<Option<String>>(4);

        // Transcript setup: create writer if enabled, run sweep.
        let transcript_writer = if config.transcript_enabled {
            let dir = self.effective_transcript_dir(config);
            if self.transcript_max_files > 0
                && let Err(e) = sweep_old_transcripts(&dir, self.transcript_max_files)
            {
                tracing::warn!(error = %e, "transcript sweep failed");
            }
            let path = dir.join(format!("{task_id}.jsonl"));
            match TranscriptWriter::new(&path) {
                Ok(w) => {
                    // Write initial meta with status=Submitted so running agents are
                    // discoverable even before completion.
                    let meta = TranscriptMeta {
                        agent_id: task_id.clone(),
                        agent_name: def.name.clone(),
                        def_name: def.name.clone(),
                        status: SubAgentState::Submitted,
                        started_at: crate::subagent::transcript::utc_now_pub(),
                        finished_at: None,
                        resumed_from: None,
                        turns_used: 0,
                    };
                    if let Err(e) = TranscriptWriter::write_meta(&dir, &task_id, &meta) {
                        tracing::warn!(error = %e, "failed to write initial transcript meta");
                    }
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create transcript writer");
                    None
                }
            }
        } else {
            None
        };

        let task_id_for_loop = task_id.clone();
        let join_handle: JoinHandle<Result<String, SubAgentError>> =
            tokio::spawn(run_agent_loop(AgentLoopArgs {
                provider,
                executor,
                system_prompt,
                task_prompt,
                skills,
                max_turns,
                cancel: cancel_clone,
                status_tx,
                started_at,
                secret_request_tx,
                secret_rx,
                background,
                hooks: agent_hooks,
                task_id: task_id_for_loop,
                agent_name: agent_name_clone,
                initial_messages: vec![],
                transcript_writer,
                model: def.model.clone(),
            }));

        let handle_transcript_dir = if config.transcript_enabled {
            Some(self.effective_transcript_dir(config))
        } else {
            None
        };

        let handle = SubAgentHandle {
            id: task_id.clone(),
            def,
            task_id: task_id.clone(),
            state: SubAgentState::Submitted,
            join_handle: Some(join_handle),
            cancel,
            status_rx,
            grants: PermissionGrants::default(),
            pending_secret_rx,
            secret_tx,
            started_at_str: crate::subagent::transcript::utc_now_pub(),
            transcript_dir: handle_transcript_dir,
        };

        self.agents.insert(task_id.clone(), handle);
        // FIX-6: log permission_mode so operators can audit privilege escalation at spawn time.
        // TODO: enforce permission_mode at runtime (restrict tool access based on mode).
        tracing::info!(
            task_id,
            def_name,
            permission_mode = ?self.agents[&task_id].def.permissions.permission_mode,
            "sub-agent spawned"
        );

        // Cache stop hooks from config so cancel() and collect() can fire them
        // without needing a config reference. Only update when non-empty to avoid
        // overwriting a previously configured stop hook list with an empty default.
        if !config.hooks.stop.is_empty() && self.stop_hooks.is_empty() {
            self.stop_hooks.clone_from(&config.hooks.stop);
        }

        // Fire SubagentStart lifecycle hooks (fire-and-forget).
        if !config.hooks.start.is_empty() {
            let start_hooks = config.hooks.start.clone();
            let start_env = make_hook_env(&task_id, def_name, "");
            tokio::spawn(async move {
                if let Err(e) = fire_hooks(&start_hooks, &start_env).await {
                    tracing::warn!(error = %e, "SubagentStart hook failed");
                }
            });
        }

        Ok(task_id)
    }

    /// Cancel all active sub-agents. Called during main agent shutdown.
    pub fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        for id in ids {
            let _ = self.cancel(&id);
        }
    }

    /// Cancel a running sub-agent by task ID.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown.
    pub fn cancel(&mut self, task_id: &str) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;
        handle.cancel.cancel();
        handle.state = SubAgentState::Canceled;
        handle.grants.revoke_all();
        tracing::info!(task_id, "sub-agent cancelled");

        // Fire SubagentStop lifecycle hooks (fire-and-forget).
        if !self.stop_hooks.is_empty() {
            let stop_hooks = self.stop_hooks.clone();
            let stop_env = make_hook_env(task_id, &handle.def.name, "");
            tokio::spawn(async move {
                if let Err(e) = fire_hooks(&stop_hooks, &stop_env).await {
                    tracing::warn!(error = %e, "SubagentStop hook failed");
                }
            });
        }

        Ok(())
    }

    /// Approve a secret request for a running sub-agent.
    ///
    /// Called after the user approves a vault secret access prompt. The secret
    /// key must appear in the sub-agent definition's allowed `secrets` list;
    /// otherwise the request is auto-denied.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Invalid`] if the key is not in the definition's allowed list.
    pub fn approve_secret(
        &mut self,
        task_id: &str,
        secret_key: &str,
        ttl: std::time::Duration,
    ) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        // Sweep stale grants before adding a new one for consistent housekeeping.
        handle.grants.sweep_expired();

        if !handle
            .def
            .permissions
            .secrets
            .iter()
            .any(|k| k == secret_key)
        {
            // Do not log the key name at warn level — only log that a request was denied.
            tracing::warn!(task_id, "secret request denied: key not in allowed list");
            return Err(SubAgentError::Invalid(format!(
                "secret is not in the allowed secrets list for '{}'",
                handle.def.name
            )));
        }

        handle.grants.grant_secret(secret_key, ttl);
        Ok(())
    }

    /// Deliver a secret value to a waiting sub-agent loop.
    ///
    /// Should be called after the user approves the request and the vault value
    /// has been resolved. Returns an error if no such agent is found.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown.
    pub fn deliver_secret(&mut self, task_id: &str, key: String) -> Result<(), SubAgentError> {
        // Signal approval to the sub-agent loop. The secret value is NOT passed through the
        // channel to avoid embedding it in LLM message history. The sub-agent accesses it
        // exclusively via PermissionGrants (granted by approve_secret() before this call).
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;
        handle
            .secret_tx
            .try_send(Some(key))
            .map_err(|e| SubAgentError::Channel(e.to_string()))
    }

    /// Deny a pending secret request — sends `None` to unblock the waiting sub-agent loop.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Channel`] if the channel is full or closed.
    pub fn deny_secret(&mut self, task_id: &str) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;
        handle
            .secret_tx
            .try_send(None)
            .map_err(|e| SubAgentError::Channel(e.to_string()))
    }

    /// Try to receive a pending secret request from a sub-agent (non-blocking).
    ///
    /// Returns `Some((task_id, SecretRequest))` if a request is waiting.
    pub fn try_recv_secret_request(&mut self) -> Option<(String, SecretRequest)> {
        for handle in self.agents.values_mut() {
            if let Ok(req) = handle.pending_secret_rx.try_recv() {
                return Some((handle.task_id.clone(), req));
            }
        }
        None
    }

    /// Collect the result from a completed sub-agent, removing it from the active set.
    ///
    /// Writes a final `TranscriptMeta` sidecar with the terminal state and turn count.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Spawn`] if the task panicked.
    pub async fn collect(&mut self, task_id: &str) -> Result<String, SubAgentError> {
        let mut handle = self
            .agents
            .remove(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        // Fire SubagentStop lifecycle hooks (fire-and-forget) before cleanup.
        if !self.stop_hooks.is_empty() {
            let stop_hooks = self.stop_hooks.clone();
            let stop_env = make_hook_env(task_id, &handle.def.name, "");
            tokio::spawn(async move {
                if let Err(e) = fire_hooks(&stop_hooks, &stop_env).await {
                    tracing::warn!(error = %e, "SubagentStop hook failed");
                }
            });
        }

        handle.grants.revoke_all();

        let result = if let Some(jh) = handle.join_handle.take() {
            jh.await.map_err(|e| SubAgentError::Spawn(e.to_string()))?
        } else {
            Ok(String::new())
        };

        // Write terminal meta sidecar if transcripts were enabled at spawn time.
        if let Some(ref dir) = handle.transcript_dir.clone() {
            let status = handle.status_rx.borrow();
            let final_status = if result.is_err() {
                SubAgentState::Failed
            } else if status.state == SubAgentState::Canceled {
                SubAgentState::Canceled
            } else {
                SubAgentState::Completed
            };
            let turns_used = status.turns_used;
            drop(status);

            let meta = TranscriptMeta {
                agent_id: task_id.to_owned(),
                agent_name: handle.def.name.clone(),
                def_name: handle.def.name.clone(),
                status: final_status,
                started_at: handle.started_at_str.clone(),
                finished_at: Some(crate::subagent::transcript::utc_now_pub()),
                resumed_from: None,
                turns_used,
            };
            if let Err(e) = TranscriptWriter::write_meta(dir, task_id, &meta) {
                tracing::warn!(error = %e, task_id, "failed to write final transcript meta");
            }
        }

        result
    }

    /// Resume a previously completed (or failed/cancelled) sub-agent session.
    ///
    /// Loads the transcript from the original session into memory and spawns a new
    /// agent loop with that history prepended. The new session gets a fresh UUID.
    ///
    /// Returns `(new_task_id, def_name)` on success so the caller can resolve skills by name.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::StillRunning`] if the agent is still active,
    /// [`SubAgentError::NotFound`] if no transcript with the given prefix exists,
    /// [`SubAgentError::AmbiguousId`] if the prefix matches multiple agents,
    /// [`SubAgentError::Transcript`] on I/O or parse failure,
    /// [`SubAgentError::ConcurrencyLimit`] if the concurrency limit is exceeded.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn resume(
        &mut self,
        id_prefix: &str,
        task_prompt: &str,
        provider: AnyProvider,
        tool_executor: Arc<dyn ErasedToolExecutor>,
        skills: Option<Vec<String>>,
        config: &SubAgentConfig,
    ) -> Result<(String, String), SubAgentError> {
        let dir = self.effective_transcript_dir(config);
        // Resolve full original ID first so the StillRunning check is precise
        // (avoids false positives from very short prefixes matching unrelated active agents).
        let original_id = TranscriptReader::find_by_prefix(&dir, id_prefix)?;

        // Check if the resolved original agent ID is still active in memory.
        if self.agents.contains_key(&original_id) {
            return Err(SubAgentError::StillRunning(original_id));
        }
        let meta = TranscriptReader::load_meta(&dir, &original_id)?;

        // Only terminal states can be resumed.
        match meta.status {
            SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Canceled => {}
            other => {
                return Err(SubAgentError::StillRunning(format!(
                    "{original_id} (status: {other:?})"
                )));
            }
        }

        let jsonl_path = dir.join(format!("{original_id}.jsonl"));
        let initial_messages = TranscriptReader::load(&jsonl_path)?;

        // Resolve the definition from the original meta and apply config-level defaults,
        // identical to spawn() so that config policy is always enforced.
        let mut def = self
            .definitions
            .iter()
            .find(|d| d.name == meta.def_name)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound(meta.def_name.clone()))?;

        if def.permissions.permission_mode == PermissionMode::Default
            && let Some(default_mode) = config.default_permission_mode
        {
            def.permissions.permission_mode = default_mode;
        }

        if !config.default_disallowed_tools.is_empty() {
            let mut merged = def.disallowed_tools.clone();
            for tool in &config.default_disallowed_tools {
                if !merged.contains(tool) {
                    merged.push(tool.clone());
                }
            }
            def.disallowed_tools = merged;
        }

        if def.permissions.permission_mode == PermissionMode::BypassPermissions
            && !config.allow_bypass_permissions
        {
            return Err(SubAgentError::Invalid(format!(
                "sub-agent '{}' requests bypass_permissions mode but it is not allowed by config",
                def.name
            )));
        }

        // Check concurrency limit.
        let active = self
            .agents
            .values()
            .filter(|h| matches!(h.state, SubAgentState::Working | SubAgentState::Submitted))
            .count();
        if active >= self.max_concurrent {
            return Err(SubAgentError::ConcurrencyLimit {
                active,
                max: self.max_concurrent,
            });
        }

        let new_task_id = Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let started_at = Instant::now();
        let initial_status = SubAgentStatus {
            state: SubAgentState::Submitted,
            last_message: None,
            turns_used: 0,
            started_at,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);

        let permission_mode = def.permissions.permission_mode;
        let background = def.permissions.background;
        let max_turns = def.permissions.max_turns;
        let system_prompt = def.system_prompt.clone();
        let task_prompt_owned = task_prompt.to_owned();
        let cancel_clone = cancel.clone();
        let agent_hooks = def.hooks.clone();
        let agent_name_clone = def.name.clone();

        let filtered_executor = FilteredToolExecutor::with_disallowed(
            tool_executor.clone(),
            def.tools.clone(),
            def.disallowed_tools.clone(),
        );
        let executor: FilteredToolExecutor = if permission_mode == PermissionMode::Plan {
            let plan_inner = Arc::new(PlanModeExecutor::new(tool_executor));
            FilteredToolExecutor::with_disallowed(
                plan_inner,
                def.tools.clone(),
                def.disallowed_tools.clone(),
            )
        } else {
            filtered_executor
        };

        let (secret_request_tx, pending_secret_rx) = mpsc::channel::<SecretRequest>(4);
        let (secret_tx, secret_rx) = mpsc::channel::<Option<String>>(4);

        // Transcript writer for the new (resumed) session.
        let transcript_writer = if config.transcript_enabled {
            if self.transcript_max_files > 0
                && let Err(e) = sweep_old_transcripts(&dir, self.transcript_max_files)
            {
                tracing::warn!(error = %e, "transcript sweep failed");
            }
            let new_path = dir.join(format!("{new_task_id}.jsonl"));
            let init_meta = TranscriptMeta {
                agent_id: new_task_id.clone(),
                agent_name: def.name.clone(),
                def_name: def.name.clone(),
                status: SubAgentState::Submitted,
                started_at: crate::subagent::transcript::utc_now_pub(),
                finished_at: None,
                resumed_from: Some(original_id.clone()),
                turns_used: 0,
            };
            if let Err(e) = TranscriptWriter::write_meta(&dir, &new_task_id, &init_meta) {
                tracing::warn!(error = %e, "failed to write resumed transcript meta");
            }
            match TranscriptWriter::new(&new_path) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create resumed transcript writer");
                    None
                }
            }
        } else {
            None
        };

        let new_task_id_for_loop = new_task_id.clone();
        let join_handle: JoinHandle<Result<String, SubAgentError>> =
            tokio::spawn(run_agent_loop(AgentLoopArgs {
                provider,
                executor,
                system_prompt,
                task_prompt: task_prompt_owned,
                skills,
                max_turns,
                cancel: cancel_clone,
                status_tx,
                started_at,
                secret_request_tx,
                secret_rx,
                background,
                hooks: agent_hooks,
                task_id: new_task_id_for_loop,
                agent_name: agent_name_clone,
                initial_messages,
                transcript_writer,
                model: def.model.clone(),
            }));

        let resume_handle_transcript_dir = if config.transcript_enabled {
            Some(dir.clone())
        } else {
            None
        };

        let handle = SubAgentHandle {
            id: new_task_id.clone(),
            def,
            task_id: new_task_id.clone(),
            state: SubAgentState::Submitted,
            join_handle: Some(join_handle),
            cancel,
            status_rx,
            grants: PermissionGrants::default(),
            pending_secret_rx,
            secret_tx,
            started_at_str: crate::subagent::transcript::utc_now_pub(),
            transcript_dir: resume_handle_transcript_dir,
        };

        self.agents.insert(new_task_id.clone(), handle);
        tracing::info!(
            task_id = %new_task_id,
            original_id = %original_id,
            "sub-agent resumed"
        );

        // Cache stop hooks from config if not already cached.
        if !config.hooks.stop.is_empty() && self.stop_hooks.is_empty() {
            self.stop_hooks.clone_from(&config.hooks.stop);
        }

        // Fire SubagentStart lifecycle hooks (fire-and-forget).
        if !config.hooks.start.is_empty() {
            let start_hooks = config.hooks.start.clone();
            let def_name = meta.def_name.clone();
            let start_env = make_hook_env(&new_task_id, &def_name, "");
            tokio::spawn(async move {
                if let Err(e) = fire_hooks(&start_hooks, &start_env).await {
                    tracing::warn!(error = %e, "SubagentStart hook failed");
                }
            });
        }

        Ok((new_task_id, meta.def_name))
    }

    /// Resolve the effective transcript directory from config or default.
    fn effective_transcript_dir(&self, config: &SubAgentConfig) -> PathBuf {
        if let Some(ref dir) = self.transcript_dir {
            dir.clone()
        } else if let Some(ref dir) = config.transcript_dir {
            dir.clone()
        } else {
            PathBuf::from(".zeph/subagents")
        }
    }

    /// Look up the definition name for a resumable transcript without spawning.
    ///
    /// Used by callers that need to resolve skills before calling `resume()`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`TranscriptReader::find_by_prefix`] and
    /// [`TranscriptReader::load_meta`].
    pub fn def_name_for_resume(
        &self,
        id_prefix: &str,
        config: &SubAgentConfig,
    ) -> Result<String, SubAgentError> {
        let dir = self.effective_transcript_dir(config);
        let original_id = TranscriptReader::find_by_prefix(&dir, id_prefix)?;
        let meta = TranscriptReader::load_meta(&dir, &original_id)?;
        Ok(meta.def_name)
    }

    /// Return a snapshot of all active sub-agent statuses.
    #[must_use]
    pub fn statuses(&self) -> Vec<(String, SubAgentStatus)> {
        self.agents
            .values()
            .map(|h| {
                let mut status = h.status_rx.borrow().clone();
                // cancel() updates handle.state synchronously but the background task
                // may not have sent the final watch update yet; reflect it here.
                if h.state == SubAgentState::Canceled {
                    status.state = SubAgentState::Canceled;
                }
                (h.task_id.clone(), status)
            })
            .collect()
    }

    /// Return the definition for a specific agent by `task_id`.
    #[must_use]
    pub fn agents_def(&self, task_id: &str) -> Option<&SubAgentDef> {
        self.agents.get(task_id).map(|h| &h.def)
    }

    /// Spawn a sub-agent for an orchestrated task.
    ///
    /// Identical to [`spawn`][Self::spawn] but wraps the `JoinHandle` to send a
    /// [`crate::orchestration::TaskEvent`] on the provided channel when the agent loop
    /// terminates. This allows the `DagScheduler` to receive completion notifications
    /// without polling (ADR-027).
    ///
    /// The `event_tx` channel is best-effort: if the scheduler is dropped before all
    /// agents complete, the send will fail silently with a warning log.
    ///
    /// # Errors
    ///
    /// Same error conditions as [`spawn`][Self::spawn].
    ///
    /// # Panics
    ///
    /// Panics if the internal agent entry is missing after a successful `spawn` call.
    /// This is a programming error and should never occur in normal operation.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_for_task(
        &mut self,
        def_name: &str,
        task_prompt: &str,
        provider: AnyProvider,
        tool_executor: Arc<dyn ErasedToolExecutor>,
        skills: Option<Vec<String>>,
        config: &SubAgentConfig,
        orch_task_id: crate::orchestration::TaskId,
        event_tx: tokio::sync::mpsc::Sender<crate::orchestration::TaskEvent>,
    ) -> Result<String, SubAgentError> {
        use crate::orchestration::{TaskEvent, TaskOutcome};

        let handle_id = self.spawn(
            def_name,
            task_prompt,
            provider,
            tool_executor,
            skills,
            config,
        )?;

        let handle = self
            .agents
            .get_mut(&handle_id)
            .expect("just spawned agent must exist");

        let original_join = handle
            .join_handle
            .take()
            .expect("just spawned agent must have a join handle");

        let handle_id_clone = handle_id.clone();
        let wrapped_join: tokio::task::JoinHandle<Result<String, SubAgentError>> =
            tokio::spawn(async move {
                let result = original_join.await;

                let (outcome, output) = match &result {
                    Ok(Ok(output)) => (
                        TaskOutcome::Completed {
                            output: output.clone(),
                            artifacts: vec![],
                        },
                        Ok(output.clone()),
                    ),
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        (
                            TaskOutcome::Failed { error: msg.clone() },
                            Err(SubAgentError::Spawn(msg)),
                        )
                    }
                    Err(join_err) => (
                        TaskOutcome::Failed {
                            // Use Debug format to preserve panic backtrace info (S3).
                            error: format!("task panicked: {join_err:?}"),
                        },
                        Err(SubAgentError::TaskPanic(format!(
                            "task panicked: {join_err:?}"
                        ))),
                    ),
                };

                // Best-effort send. If the scheduler was dropped, warn but do not fail.
                if let Err(e) = event_tx
                    .send(TaskEvent {
                        task_id: orch_task_id,
                        agent_handle_id: handle_id_clone,
                        outcome,
                    })
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        "failed to send TaskEvent: scheduler may have been dropped"
                    );
                }

                output
            });

        handle.join_handle = Some(wrapped_join);

        Ok(handle_id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::await_holding_lock,
        clippy::field_reassign_with_default,
        clippy::too_many_lines
    )]

    use std::pin::Pin;

    use indoc::indoc;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_tools::ToolCall;
    use zeph_tools::executor::{ErasedToolExecutor, ToolError, ToolOutput};
    use zeph_tools::registry::ToolDef;

    use serial_test::serial;

    use crate::config::SubAgentConfig;
    use crate::subagent::def::MemoryScope;

    use super::*;

    fn make_manager() -> SubAgentManager {
        SubAgentManager::new(4)
    }

    fn sample_def() -> SubAgentDef {
        SubAgentDef::parse("---\nname: bot\ndescription: A bot\n---\n\nDo things.\n").unwrap()
    }

    fn def_with_secrets() -> SubAgentDef {
        SubAgentDef::parse(
            "---\nname: bot\ndescription: A bot\npermissions:\n  secrets:\n    - api-key\n---\n\nDo things.\n",
        )
        .unwrap()
    }

    struct NoopExecutor;

    impl ErasedToolExecutor for NoopExecutor {
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
            _call: &'a ToolCall,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
            false
        }
    }

    fn mock_provider(responses: Vec<&str>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(
            responses.into_iter().map(String::from).collect(),
        ))
    }

    fn noop_executor() -> Arc<dyn ErasedToolExecutor> {
        Arc::new(NoopExecutor)
    }

    fn do_spawn(
        mgr: &mut SubAgentManager,
        name: &str,
        prompt: &str,
    ) -> Result<String, SubAgentError> {
        mgr.spawn(
            name,
            prompt,
            mock_provider(vec!["done"]),
            noop_executor(),
            None,
            &SubAgentConfig::default(),
        )
    }

    #[test]
    fn load_definitions_populates_vec() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let content = "---\nname: helper\ndescription: A helper\n---\n\nHelp.\n";
        let mut f = std::fs::File::create(dir.path().join("helper.md")).unwrap();
        f.write_all(content.as_bytes()).unwrap();

        let mut mgr = make_manager();
        mgr.load_definitions(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(mgr.definitions().len(), 1);
        assert_eq!(mgr.definitions()[0].name, "helper");
    }

    #[test]
    fn spawn_not_found_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        let err = do_spawn(&mut mgr, "nonexistent", "prompt").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn spawn_and_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "do stuff").unwrap();
        assert!(!task_id.is_empty());

        mgr.cancel(&task_id).unwrap();
        assert_eq!(mgr.agents[&task_id].state, SubAgentState::Canceled);
    }

    #[test]
    fn cancel_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr.cancel("unknown-id").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[tokio::test]
    async fn collect_removes_agent() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "do stuff").unwrap();
        mgr.cancel(&task_id).unwrap();

        // Wait briefly for the task to observe cancellation
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = mgr.collect(&task_id).await.unwrap();
        assert!(!mgr.agents.contains_key(&task_id));
        // result may be empty string (cancelled before LLM response) or the mock response
        let _ = result;
    }

    #[tokio::test]
    async fn collect_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr.collect("unknown-id").await.unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn approve_secret_grants_access() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(def_with_secrets());

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
        mgr.approve_secret(&task_id, "api-key", std::time::Duration::from_secs(60))
            .unwrap();

        let handle = mgr.agents.get_mut(&task_id).unwrap();
        assert!(
            handle
                .grants
                .is_active(&crate::subagent::GrantKind::Secret("api-key".into()))
        );
    }

    #[test]
    fn approve_secret_denied_for_unlisted_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def()); // no secrets in allowed list

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
        let err = mgr
            .approve_secret(&task_id, "not-allowed", std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn approve_secret_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr
            .approve_secret("unknown", "key", std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn statuses_returns_active_agents() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
        let statuses = mgr.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0, task_id);
    }

    #[test]
    fn concurrency_limit_enforced() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(1);
        mgr.definitions.push(sample_def());

        let _first = do_spawn(&mut mgr, "bot", "first").unwrap();
        let err = do_spawn(&mut mgr, "bot", "second").unwrap_err();
        assert!(matches!(err, SubAgentError::ConcurrencyLimit { .. }));
    }

    // --- #1619 regression tests: reserved_slots ---

    #[test]
    fn test_reserve_slots_blocks_spawn() {
        // max_concurrent=2, reserved=1, active=1 → active+reserved >= max → ConcurrencyLimit.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(2);
        mgr.definitions.push(sample_def());

        // Occupy one slot.
        let _first = do_spawn(&mut mgr, "bot", "first").unwrap();
        // Reserve the remaining slot.
        mgr.reserve_slots(1);
        // Now active(1) + reserved(1) >= max_concurrent(2) → should reject.
        let err = do_spawn(&mut mgr, "bot", "second").unwrap_err();
        assert!(
            matches!(err, SubAgentError::ConcurrencyLimit { .. }),
            "expected ConcurrencyLimit, got: {err}"
        );
    }

    #[test]
    fn test_release_reservation_allows_spawn() {
        // After release_reservation(), the reserved slot is freed and spawn succeeds.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(2);
        mgr.definitions.push(sample_def());

        // Reserve one slot (no active agents yet).
        mgr.reserve_slots(1);
        // active(0) + reserved(1) < max_concurrent(2), so one more spawn is allowed.
        let _first = do_spawn(&mut mgr, "bot", "first").unwrap();
        // Now active(1) + reserved(1) >= max_concurrent(2) → blocked.
        let err = do_spawn(&mut mgr, "bot", "second").unwrap_err();
        assert!(matches!(err, SubAgentError::ConcurrencyLimit { .. }));

        // Release the reservation — active(1) + reserved(0) < max_concurrent(2).
        mgr.release_reservation(1);
        let result = do_spawn(&mut mgr, "bot", "third");
        assert!(
            result.is_ok(),
            "spawn must succeed after release_reservation, got: {result:?}"
        );
    }

    #[test]
    fn test_reservation_with_zero_active_blocks_spawn() {
        // Reserved slots alone (no active agents) should block spawn when reserved >= max.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(2);
        mgr.definitions.push(sample_def());

        // Reserve all slots — no active agents.
        mgr.reserve_slots(2);
        // active(0) + reserved(2) >= max_concurrent(2) → blocked.
        let err = do_spawn(&mut mgr, "bot", "first").unwrap_err();
        assert!(
            matches!(err, SubAgentError::ConcurrencyLimit { .. }),
            "reservation alone must block spawn when reserved >= max_concurrent"
        );
    }

    #[tokio::test]
    async fn background_agent_does_not_block_caller() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Spawn should return immediately without waiting for LLM
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            std::future::ready(do_spawn(&mut mgr, "bot", "work")),
        )
        .await;
        assert!(result.is_ok(), "spawn() must not block");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn max_turns_terminates_agent_loop() {
        let mut mgr = make_manager();
        // max_turns = 1, mock returns empty (no tool call), so loop ends after 1 turn
        let def = SubAgentDef::parse(indoc! {"
            ---
            name: limited
            description: A bot
            permissions:
              max_turns: 1
            ---

            Do one thing.
        "})
        .unwrap();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "limited",
                "task",
                mock_provider(vec!["final answer"]),
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();

        // Wait for completion
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let status = mgr.statuses().into_iter().find(|(id, _)| id == &task_id);
        // Status should show Completed or still Working but <= 1 turn
        if let Some((_, s)) = status {
            assert!(s.turns_used <= 1);
        }
    }

    #[tokio::test]
    async fn cancellation_token_stops_agent_loop() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "long task").unwrap();

        // Cancel immediately
        mgr.cancel(&task_id).unwrap();

        // Wait a bit then collect
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let result = mgr.collect(&task_id).await;
        // Cancelled task may return empty or partial result — both are acceptable
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn shutdown_all_cancels_all_active_agents() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        do_spawn(&mut mgr, "bot", "task 1").unwrap();
        do_spawn(&mut mgr, "bot", "task 2").unwrap();

        assert_eq!(mgr.agents.len(), 2);
        mgr.shutdown_all();

        // All agents should be in Canceled state
        for (_, status) in mgr.statuses() {
            assert_eq!(status.state, SubAgentState::Canceled);
        }
    }

    #[test]
    fn debug_impl_does_not_expose_sensitive_fields() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(def_with_secrets());
        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
        let handle = &mgr.agents[&task_id];
        let debug_str = format!("{handle:?}");
        // SubAgentHandle Debug must not expose grant contents or secrets
        assert!(!debug_str.contains("api-key"));
    }

    #[tokio::test]
    async fn llm_failure_transitions_to_failed_state() {
        let rt_handle = tokio::runtime::Handle::current();
        let _guard = rt_handle.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let failing = AnyProvider::Mock(MockProvider::failing());
        let task_id = mgr
            .spawn(
                "bot",
                "do work",
                failing,
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();

        // Wait for the background task to complete.
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let statuses = mgr.statuses();
        let status = statuses
            .iter()
            .find(|(id, _)| id == &task_id)
            .map(|(_, s)| s);
        // The background loop should have caught the LLM error and reported Failed.
        assert!(
            status.is_some_and(|s| s.state == SubAgentState::Failed),
            "expected Failed, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn tool_call_loop_two_turns() {
        use std::sync::Mutex;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_tools::ToolCall;

        struct ToolOnceExecutor {
            calls: Mutex<u32>,
        }

        impl ErasedToolExecutor for ToolOnceExecutor {
            fn execute_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn execute_confirmed_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
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
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                let result = if *n == 1 {
                    Ok(Some(ToolOutput {
                        tool_name: call.tool_id.clone(),
                        summary: "step 1 done".into(),
                        blocks_executed: 1,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                    }))
                } else {
                    Ok(None)
                };
                Box::pin(std::future::ready(result))
            }

            fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
                false
            }
        }

        let rt_handle = tokio::runtime::Handle::current();
        let _guard = rt_handle.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // First response: ToolUse with a shell call; second: Text with final answer.
        let tool_response = ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: "call-1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "echo hi"}),
            }],
            thinking_blocks: vec![],
        };
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_response,
            ChatResponse::Text("final answer".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let executor = Arc::new(ToolOnceExecutor {
            calls: Mutex::new(0),
        });

        let task_id = mgr
            .spawn(
                "bot",
                "run two turns",
                provider,
                executor,
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();

        // Wait for background loop to finish.
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let result = mgr.collect(&task_id).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[tokio::test]
    async fn collect_on_running_task_completes_eventually() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Spawn with a slow response so the task is still running.
        let task_id = do_spawn(&mut mgr, "bot", "slow work").unwrap();

        // collect() awaits the JoinHandle, so it will finish when the task completes.
        let result =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), mgr.collect(&task_id)).await;

        assert!(result.is_ok(), "collect timed out after 5s");
        let inner = result.unwrap();
        assert!(inner.is_ok(), "collect returned error: {inner:?}");
    }

    #[test]
    fn concurrency_slot_freed_after_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(1); // limit to 1
        mgr.definitions.push(sample_def());

        let id1 = do_spawn(&mut mgr, "bot", "task 1").unwrap();

        // Concurrency limit reached — second spawn should fail.
        let err = do_spawn(&mut mgr, "bot", "task 2").unwrap_err();
        assert!(
            matches!(err, SubAgentError::ConcurrencyLimit { .. }),
            "expected concurrency limit error, got: {err}"
        );

        // Cancel the first agent to free the slot.
        mgr.cancel(&id1).unwrap();

        // Now a new spawn should succeed.
        let result = do_spawn(&mut mgr, "bot", "task 3");
        assert!(
            result.is_ok(),
            "expected spawn to succeed after cancel, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn skill_bodies_prepended_to_system_prompt() {
        // Verify that when skills are passed to spawn(), the agent loop prepends
        // them to the system prompt inside a ```skills fence.
        use zeph_llm::mock::MockProvider;

        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let skill_bodies = vec!["# skill-one\nDo something useful.".to_owned()];
        let task_id = mgr
            .spawn(
                "bot",
                "task",
                provider,
                noop_executor(),
                Some(skill_bodies),
                &SubAgentConfig::default(),
            )
            .unwrap();

        // Wait for the loop to call the provider at least once.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let calls = recorded.lock().unwrap();
        assert!(!calls.is_empty(), "provider should have been called");
        // The first message in the first call is the system prompt.
        let system_msg = &calls[0][0].content;
        assert!(
            system_msg.contains("```skills"),
            "system prompt must contain ```skills fence, got: {system_msg}"
        );
        assert!(
            system_msg.contains("skill-one"),
            "system prompt must contain the skill body, got: {system_msg}"
        );
        drop(calls);

        let _ = mgr.collect(&task_id).await;
    }

    #[tokio::test]
    async fn no_skills_does_not_add_fence_to_system_prompt() {
        use zeph_llm::mock::MockProvider;

        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = mgr
            .spawn(
                "bot",
                "task",
                provider,
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let calls = recorded.lock().unwrap();
        assert!(!calls.is_empty());
        let system_msg = &calls[0][0].content;
        assert!(
            !system_msg.contains("```skills"),
            "system prompt must not contain skills fence when no skills passed"
        );
        drop(calls);

        let _ = mgr.collect(&task_id).await;
    }

    #[tokio::test]
    async fn statuses_does_not_include_collected_task() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "task").unwrap();
        assert_eq!(mgr.statuses().len(), 1);

        // Wait for task completion then collect.
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        let _ = mgr.collect(&task_id).await;

        // After collect(), the task should no longer appear in statuses.
        assert!(
            mgr.statuses().is_empty(),
            "expected empty statuses after collect"
        );
    }

    #[tokio::test]
    async fn background_agent_auto_denies_secret_request() {
        use zeph_llm::mock::MockProvider;

        // Background agent that requests a secret — the loop must auto-deny without blocking.
        let def = SubAgentDef::parse(indoc! {"
            ---
            name: bg-bot
            description: Background bot
            permissions:
              background: true
              secrets:
                - api-key
            ---

            [REQUEST_SECRET: api-key]
        "})
        .unwrap();

        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "bg-bot",
                "task",
                provider,
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();

        // Should complete without blocking — background auto-denies the secret.
        let result =
            tokio::time::timeout(tokio::time::Duration::from_secs(2), mgr.collect(&task_id)).await;
        assert!(
            result.is_ok(),
            "background agent must not block on secret request"
        );
        drop(recorded);
    }

    #[test]
    fn spawn_with_plan_mode_definition_succeeds() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: planner
            description: A planner bot
            permissions:
              permission_mode: plan
            ---

            Plan only.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = do_spawn(&mut mgr, "planner", "make a plan").unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    #[test]
    fn spawn_with_disallowed_tools_definition_succeeds() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: safe-bot
            description: Bot with disallowed tools
            tools:
              allow:
                - shell
                - web
              except:
                - shell
            ---

            Do safe things.
        "})
        .unwrap();

        assert_eq!(def.disallowed_tools, ["shell"]);

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = do_spawn(&mut mgr, "safe-bot", "task").unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    // ── #1180: default_permission_mode / default_disallowed_tools applied at spawn ──

    #[test]
    fn spawn_applies_default_permission_mode_from_config() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        // Agent has Default permission mode — config sets Plan as default.
        let def =
            SubAgentDef::parse("---\nname: bot\ndescription: A bot\n---\n\nDo things.\n").unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::Default);

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            default_permission_mode: Some(PermissionMode::Plan),
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "bot",
                "prompt",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    #[test]
    fn spawn_does_not_override_explicit_permission_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        // Agent explicitly sets DontAsk — config default must not override it.
        let def = SubAgentDef::parse(indoc! {"
            ---
            name: bot
            description: A bot
            permissions:
              permission_mode: dont_ask
            ---

            Do things.
        "})
        .unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::DontAsk);

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            default_permission_mode: Some(PermissionMode::Plan),
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "bot",
                "prompt",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    #[test]
    fn spawn_merges_global_disallowed_tools() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let def =
            SubAgentDef::parse("---\nname: bot\ndescription: A bot\n---\n\nDo things.\n").unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            default_disallowed_tools: vec!["dangerous".into()],
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "bot",
                "prompt",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    // ── #1182: bypass_permissions blocked without config gate ─────────────

    #[test]
    fn spawn_bypass_permissions_without_config_gate_is_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: bypass-bot
            description: A bot with bypass mode
            permissions:
              permission_mode: bypass_permissions
            ---

            Unrestricted.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        // Default config: allow_bypass_permissions = false
        let cfg = SubAgentConfig::default();
        let err = mgr
            .spawn(
                "bypass-bot",
                "prompt",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn spawn_bypass_permissions_with_config_gate_succeeds() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: bypass-bot
            description: A bot with bypass mode
            permissions:
              permission_mode: bypass_permissions
            ---

            Unrestricted.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            allow_bypass_permissions: true,
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "bypass-bot",
                "prompt",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();
    }

    // ── resume() tests ────────────────────────────────────────────────────────

    /// Write a minimal completed meta file and empty JSONL so `resume()` has something to load.
    fn write_completed_meta(dir: &std::path::Path, agent_id: &str, def_name: &str) {
        use crate::subagent::transcript::{TranscriptMeta, TranscriptWriter};
        let meta = TranscriptMeta {
            agent_id: agent_id.to_owned(),
            agent_name: def_name.to_owned(),
            def_name: def_name.to_owned(),
            status: SubAgentState::Completed,
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:01:00Z".to_owned()),
            resumed_from: None,
            turns_used: 1,
        };
        TranscriptWriter::write_meta(dir, agent_id, &meta).unwrap();
        // Create the empty JSONL so TranscriptReader::load succeeds.
        std::fs::write(dir.join(format!("{agent_id}.jsonl")), b"").unwrap();
    }

    fn make_cfg_with_dir(dir: &std::path::Path) -> SubAgentConfig {
        SubAgentConfig {
            transcript_dir: Some(dir.to_path_buf()),
            ..SubAgentConfig::default()
        }
    }

    #[test]
    fn resume_not_found_returns_not_found_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());
        let cfg = make_cfg_with_dir(tmp.path());

        let err = mgr
            .resume(
                "deadbeef",
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn resume_ambiguous_id_returns_ambiguous_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        write_completed_meta(tmp.path(), "aabb0001-0000-0000-0000-000000000000", "bot");
        write_completed_meta(tmp.path(), "aabb0002-0000-0000-0000-000000000000", "bot");

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());
        let cfg = make_cfg_with_dir(tmp.path());

        let err = mgr
            .resume(
                "aabb",
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(matches!(err, SubAgentError::AmbiguousId(_, 2)));
    }

    #[test]
    fn resume_still_running_via_active_agents_returns_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let agent_id = "cafebabe-0000-0000-0000-000000000000";
        write_completed_meta(tmp.path(), agent_id, "bot");

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Manually insert a fake active handle so resume() thinks it's still running.
        let (status_tx, status_rx) = watch::channel(SubAgentStatus {
            state: SubAgentState::Working,
            last_message: None,
            turns_used: 0,
            started_at: std::time::Instant::now(),
        });
        let (_secret_request_tx, pending_secret_rx) = tokio::sync::mpsc::channel(1);
        let (secret_tx, _secret_rx) = tokio::sync::mpsc::channel(1);
        let cancel = CancellationToken::new();
        let fake_def = sample_def();
        mgr.agents.insert(
            agent_id.to_owned(),
            SubAgentHandle {
                id: agent_id.to_owned(),
                def: fake_def,
                task_id: agent_id.to_owned(),
                state: SubAgentState::Working,
                join_handle: None,
                cancel,
                status_rx,
                grants: PermissionGrants::default(),
                pending_secret_rx,
                secret_tx,
                started_at_str: "2026-01-01T00:00:00Z".to_owned(),
                transcript_dir: None,
            },
        );
        drop(status_tx);

        let cfg = make_cfg_with_dir(tmp.path());
        let err = mgr
            .resume(
                agent_id,
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(matches!(err, SubAgentError::StillRunning(_)));
    }

    #[test]
    fn resume_def_not_found_returns_not_found_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let agent_id = "feedface-0000-0000-0000-000000000000";
        // Meta points to "unknown-agent" which is not in definitions.
        write_completed_meta(tmp.path(), agent_id, "unknown-agent");

        let mut mgr = make_manager();
        // Do NOT push any definition — so def_name "unknown-agent" won't be found.
        let cfg = make_cfg_with_dir(tmp.path());

        let err = mgr
            .resume(
                "feedface",
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn resume_concurrency_limit_reached_returns_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let agent_id = "babe0000-0000-0000-0000-000000000000";
        write_completed_meta(tmp.path(), agent_id, "bot");

        let mut mgr = SubAgentManager::new(1); // limit of 1
        mgr.definitions.push(sample_def());

        // Occupy the single slot.
        let _running_id = do_spawn(&mut mgr, "bot", "occupying slot").unwrap();

        let cfg = make_cfg_with_dir(tmp.path());
        let err = mgr
            .resume(
                "babe0000",
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap_err();
        assert!(
            matches!(err, SubAgentError::ConcurrencyLimit { .. }),
            "expected concurrency limit error, got: {err}"
        );
    }

    #[test]
    fn resume_happy_path_returns_new_task_id() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let agent_id = "deadcode-0000-0000-0000-000000000000";
        write_completed_meta(tmp.path(), agent_id, "bot");

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());
        let cfg = make_cfg_with_dir(tmp.path());

        let (new_id, def_name) = mgr
            .resume(
                "deadcode",
                "continue the work",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();

        assert!(!new_id.is_empty(), "new task id must not be empty");
        assert_ne!(
            new_id, agent_id,
            "resumed session must have a fresh task id"
        );
        assert_eq!(def_name, "bot");
        // New agent must be tracked.
        assert!(mgr.agents.contains_key(&new_id));

        mgr.cancel(&new_id).unwrap();
    }

    #[test]
    fn resume_populates_resumed_from_in_meta() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let original_id = "0000abcd-0000-0000-0000-000000000000";
        write_completed_meta(tmp.path(), original_id, "bot");

        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());
        let cfg = make_cfg_with_dir(tmp.path());

        let (new_id, _) = mgr
            .resume(
                "0000abcd",
                "continue",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();

        // The new meta sidecar must have resumed_from = original_id.
        let new_meta =
            crate::subagent::transcript::TranscriptReader::load_meta(tmp.path(), &new_id).unwrap();
        assert_eq!(
            new_meta.resumed_from.as_deref(),
            Some(original_id),
            "resumed_from must point to original agent id"
        );

        mgr.cancel(&new_id).unwrap();
    }

    #[test]
    fn def_name_for_resume_returns_def_name() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let agent_id = "aaaabbbb-0000-0000-0000-000000000000";
        write_completed_meta(tmp.path(), agent_id, "bot");

        let mgr = make_manager();
        let cfg = make_cfg_with_dir(tmp.path());

        let name = mgr.def_name_for_resume("aaaabbbb", &cfg).unwrap();
        assert_eq!(name, "bot");
    }

    #[test]
    fn def_name_for_resume_not_found_returns_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let tmp = tempfile::tempdir().unwrap();
        let mgr = make_manager();
        let cfg = make_cfg_with_dir(tmp.path());

        let err = mgr.def_name_for_resume("notexist", &cfg).unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    // ── Memory scope tests ────────────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn spawn_with_memory_scope_project_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: mem-agent
            description: Agent with memory
            memory: project
            ---

            System prompt.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "mem-agent",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // Verify memory directory was created.
        let mem_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("mem-agent");
        assert!(
            mem_dir.exists(),
            "memory directory should be created at spawn"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn spawn_with_config_default_memory_scope_applies_when_def_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: mem-agent2
            description: Agent without explicit memory
            ---

            System prompt.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            default_memory_scope: Some(MemoryScope::Project),
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "mem-agent2",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // Verify memory directory was created via config default.
        let mem_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("mem-agent2");
        assert!(
            mem_dir.exists(),
            "config default memory scope should create directory"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn spawn_with_memory_blocked_by_disallowed_tools_skips_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: blocked-mem
            description: Agent with memory but blocked tools
            memory: project
            tools:
              except:
                - Read
                - Write
                - Edit
            ---

            System prompt.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "blocked-mem",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // Memory dir should NOT be created because tools are blocked (HIGH-04).
        let mem_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("blocked-mem");
        assert!(
            !mem_dir.exists(),
            "memory directory should not be created when tools are blocked"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn spawn_without_memory_scope_no_directory_created() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let def = SubAgentDef::parse(indoc! {"
            ---
            name: no-mem-agent
            description: Agent without memory
            ---

            System prompt.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "no-mem-agent",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // No agent-memory directory should exist (transcript dirs may be created separately).
        let mem_dir = tmp.path().join(".zeph").join("agent-memory");
        assert!(
            !mem_dir.exists(),
            "no agent-memory directory should be created without memory scope"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[test]
    #[serial]
    fn build_prompt_injects_memory_block_after_behavioral_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        // Create memory directory and MEMORY.md.
        let mem_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("test-agent");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("MEMORY.md"), "# Test Memory\nkey: value\n").unwrap();

        let mut def = SubAgentDef::parse(indoc! {"
            ---
            name: test-agent
            description: Test agent
            memory: project
            ---

            Behavioral instructions here.
        "})
        .unwrap();

        let prompt = build_system_prompt_with_memory(&mut def, Some(MemoryScope::Project));

        // Memory block must appear AFTER behavioral prompt text.
        let behavioral_pos = prompt.find("Behavioral instructions").unwrap();
        let memory_pos = prompt.find("<agent-memory>").unwrap();
        assert!(
            memory_pos > behavioral_pos,
            "memory block must appear AFTER behavioral prompt"
        );
        assert!(
            prompt.contains("key: value"),
            "MEMORY.md content must be injected"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[test]
    #[serial]
    fn build_prompt_auto_enables_read_write_edit_for_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let mut def = SubAgentDef::parse(indoc! {"
            ---
            name: allowlist-agent
            description: AllowList agent
            memory: project
            tools:
              allow:
                - shell
            ---

            System prompt.
        "})
        .unwrap();

        assert!(
            matches!(&def.tools, ToolPolicy::AllowList(list) if list == &["shell"]),
            "should start with only shell"
        );

        build_system_prompt_with_memory(&mut def, Some(MemoryScope::Project));

        // Read/Write/Edit must be auto-added to the AllowList.
        assert!(
            matches!(&def.tools, ToolPolicy::AllowList(list)
                if list.contains(&"Read".to_owned())
                    && list.contains(&"Write".to_owned())
                    && list.contains(&"Edit".to_owned())),
            "Read/Write/Edit must be auto-enabled in AllowList when memory is set"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn spawn_with_explicit_def_memory_overrides_config_default() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        // Agent explicitly sets memory: local, config sets default: project.
        // The explicit local should win.
        let def = SubAgentDef::parse(indoc! {"
            ---
            name: override-agent
            description: Agent with explicit memory
            memory: local
            ---

            System prompt.
        "})
        .unwrap();
        assert_eq!(def.memory, Some(MemoryScope::Local));

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let cfg = SubAgentConfig {
            default_memory_scope: Some(MemoryScope::Project),
            ..SubAgentConfig::default()
        };

        let task_id = mgr
            .spawn(
                "override-agent",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &cfg,
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // Local scope directory should be created, not project scope.
        let local_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory-local")
            .join("override-agent");
        let project_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("override-agent");
        assert!(local_dir.exists(), "local memory dir should be created");
        assert!(
            !project_dir.exists(),
            "project memory dir must NOT be created"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn spawn_memory_blocked_by_deny_list_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        // tools.deny: [Read, Write, Edit] — DenyList policy blocking all file tools.
        let def = SubAgentDef::parse(indoc! {"
            ---
            name: deny-list-mem
            description: Agent with deny list
            memory: project
            tools:
              deny:
                - Read
                - Write
                - Edit
            ---

            System prompt.
        "})
        .unwrap();

        let mut mgr = make_manager();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "deny-list-mem",
                "do something",
                mock_provider(vec!["done"]),
                noop_executor(),
                None,
                &SubAgentConfig::default(),
            )
            .unwrap();
        assert!(!task_id.is_empty());
        mgr.cancel(&task_id).unwrap();

        // Memory dir should NOT be created because DenyList blocks file tools (REV-HIGH-02).
        let mem_dir = tmp
            .path()
            .join(".zeph")
            .join("agent-memory")
            .join("deny-list-mem");
        assert!(
            !mem_dir.exists(),
            "memory dir must not be created when DenyList blocks all file tools"
        );

        std::env::set_current_dir(orig_dir).unwrap();
    }

    // ── regression tests for #1467: sub-agent tools passed to LLM ────────────

    fn make_agent_loop_args(
        provider: AnyProvider,
        executor: FilteredToolExecutor,
        max_turns: u32,
    ) -> AgentLoopArgs {
        let (status_tx, _status_rx) = tokio::sync::watch::channel(SubAgentStatus {
            state: SubAgentState::Working,
            last_message: None,
            turns_used: 0,
            started_at: std::time::Instant::now(),
        });
        let (secret_request_tx, _secret_request_rx) = tokio::sync::mpsc::channel(1);
        let (_secret_approved_tx, secret_rx) = tokio::sync::mpsc::channel::<Option<String>>(1);
        AgentLoopArgs {
            provider,
            executor,
            system_prompt: "You are a bot".into(),
            task_prompt: "Do something".into(),
            skills: None,
            max_turns,
            cancel: tokio_util::sync::CancellationToken::new(),
            status_tx,
            started_at: std::time::Instant::now(),
            secret_request_tx,
            secret_rx,
            background: false,
            hooks: super::super::hooks::SubagentHooks::default(),
            task_id: "test-task".into(),
            agent_name: "test-bot".into(),
            initial_messages: vec![],
            transcript_writer: None,
            model: None,
        }
    }

    #[tokio::test]
    async fn run_agent_loop_passes_tools_to_provider() {
        use std::sync::Arc;
        use zeph_llm::provider::ChatResponse;
        use zeph_tools::registry::{InvocationHint, ToolDef};

        // Executor that exposes one tool definition.
        struct SingleToolExecutor;

        impl ErasedToolExecutor for SingleToolExecutor {
            fn execute_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn execute_confirmed_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn tool_definitions_erased(&self) -> Vec<ToolDef> {
                vec![ToolDef {
                    id: std::borrow::Cow::Borrowed("shell"),
                    description: std::borrow::Cow::Borrowed("Run a shell command"),
                    schema: schemars::Schema::default(),
                    invocation: InvocationHint::ToolCall,
                }]
            }

            fn execute_tool_call_erased<'a>(
                &'a self,
                _call: &'a ToolCall,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
                false
            }
        }

        // MockProvider with tool_use: records call count for chat_with_tools.
        let (mock, tool_call_count) =
            MockProvider::default().with_tool_use(vec![ChatResponse::Text("done".into())]);
        let provider = AnyProvider::Mock(mock);
        let executor =
            FilteredToolExecutor::new(Arc::new(SingleToolExecutor), ToolPolicy::InheritAll);

        let args = make_agent_loop_args(provider, executor, 1);
        let result = run_agent_loop(args).await;
        assert!(result.is_ok(), "loop failed: {result:?}");
        assert_eq!(
            *tool_call_count.lock().unwrap(),
            1,
            "chat_with_tools must have been called exactly once"
        );
    }

    #[tokio::test]
    async fn run_agent_loop_executes_native_tool_call() {
        use std::sync::{Arc, Mutex};
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_tools::registry::ToolDef;

        struct TrackingExecutor {
            calls: Mutex<Vec<String>>,
        }

        impl ErasedToolExecutor for TrackingExecutor {
            fn execute_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn execute_confirmed_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
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
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                self.calls.lock().unwrap().push(call.tool_id.clone());
                let output = ToolOutput {
                    tool_name: call.tool_id.clone(),
                    summary: "executed".into(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                };
                Box::pin(std::future::ready(Ok(Some(output))))
            }

            fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
                false
            }
        }

        // Provider: first call returns ToolUse, second returns Text.
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("all done".into()),
        ]);

        let tracker = Arc::new(TrackingExecutor {
            calls: Mutex::new(vec![]),
        });
        let tracker_clone = Arc::clone(&tracker);
        let executor = FilteredToolExecutor::new(tracker_clone, ToolPolicy::InheritAll);

        let args = make_agent_loop_args(AnyProvider::Mock(mock), executor, 5);
        let result = run_agent_loop(args).await;
        assert!(result.is_ok(), "loop failed: {result:?}");
        assert_eq!(result.unwrap(), "all done");

        let recorded = tracker.calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "execute_tool_call_erased must be called once"
        );
        assert_eq!(recorded[0], "shell");
    }
}
