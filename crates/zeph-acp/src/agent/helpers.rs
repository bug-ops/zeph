// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use agent_client_protocol as acp;

use zeph_core::LoopbackEvent;

pub(super) fn content_chunk_text(chunk: &acp::ContentChunk) -> String {
    match &chunk.content {
        acp::ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

pub(super) fn session_update_to_event(update: &acp::SessionUpdate) -> (&'static str, String) {
    match update {
        acp::SessionUpdate::UserMessageChunk(c) => ("user_message", content_chunk_text(c)),
        acp::SessionUpdate::AgentMessageChunk(c) => ("agent_message", content_chunk_text(c)),
        acp::SessionUpdate::AgentThoughtChunk(c) => ("agent_thought", content_chunk_text(c)),
        acp::SessionUpdate::ToolCall(tc) => {
            let payload = match serde_json::to_string(tc) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to serialize ToolCall for persistence");
                    String::new()
                }
            };
            ("tool_call", payload)
        }
        acp::SessionUpdate::ToolCallUpdate(tcu) => {
            let payload = match serde_json::to_string(tcu) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to serialize ToolCallUpdate for persistence");
                    String::new()
                }
            };
            ("tool_call_update", payload)
        }
        acp::SessionUpdate::ConfigOptionUpdate(u) => {
            let payload = serde_json::to_string(u).unwrap_or_default();
            ("config_option_update", payload)
        }
        _ => ("unknown", String::new()),
    }
}

/// Returns `true` if `text` looks like a raw tool-use marker that should not be
/// forwarded to the IDE (e.g. `[tool_use: bash (toolu_abc123)]`).
pub(super) fn is_tool_use_marker(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("[tool_use:") && trimmed.ends_with(']')
}

pub(super) fn mime_to_ext(mime: &str) -> &str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

pub(super) fn tool_kind_from_name(name: &str) -> acp::ToolKind {
    match name {
        "bash" | "shell" => acp::ToolKind::Execute,
        "read_file" => acp::ToolKind::Read,
        "write_file" => acp::ToolKind::Edit,
        "list_directory" | "find_path" | "search" | "search_code" | "grep" | "find" | "glob" => {
            acp::ToolKind::Search
        }
        "web_scrape" | "fetch" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

pub(super) const DEFAULT_MODE_ID: &str = "code";

/// MIME type used by Zed IDE to deliver LSP diagnostics as embedded resource blocks.
pub(super) const DIAGNOSTICS_MIME_TYPE: &str = "application/vnd.zed.diagnostics+json";

/// Deserialize Zed LSP diagnostics JSON and append a formatted `<diagnostics>` block to `out`.
///
pub(super) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Each entry is rendered as `file:line: [SEVERITY] message`.
/// On parse error the block is emitted empty to avoid injecting untrusted raw JSON into the prompt.
pub(super) fn format_diagnostics_block(json: &str, out: &mut String) {
    #[derive(serde::Deserialize)]
    struct DiagEntry {
        path: Option<String>,
        row: Option<u32>,
        severity: Option<String>,
        message: Option<String>,
    }

    out.push_str("<diagnostics>\n");
    match serde_json::from_str::<Vec<DiagEntry>>(json) {
        Ok(entries) => {
            for entry in entries {
                let path = entry
                    .path
                    .as_deref()
                    .map_or_else(|| "<unknown>".to_owned(), xml_escape);
                let row = entry.row.map_or_else(|| "?".to_owned(), |r| r.to_string());
                let sev = entry
                    .severity
                    .as_deref()
                    .map_or_else(|| "?".to_owned(), xml_escape);
                let msg = entry
                    .message
                    .as_deref()
                    .map_or_else(String::new, xml_escape);
                out.push_str(&path);
                out.push(':');
                out.push_str(&row);
                out.push_str(": [");
                out.push_str(&sev);
                out.push_str("] ");
                out.push_str(&msg);
                out.push('\n');
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "failed to parse diagnostics JSON — skipping");
        }
    }
    out.push_str("</diagnostics>");
}

pub(super) fn build_available_commands() -> Vec<acp::AvailableCommand> {
    vec![
        acp::AvailableCommand::new("help", "Show available commands"),
        acp::AvailableCommand::new("model", "Switch the active model").input(
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                "model id",
            )),
        ),
        acp::AvailableCommand::new("mode", "Switch session mode (code/architect/ask)").input(
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                "code | architect | ask",
            )),
        ),
        acp::AvailableCommand::new("clear", "Clear session history"),
        acp::AvailableCommand::new("compact", "Summarize and compact context"),
        acp::AvailableCommand::new("review", "Review recent changes (read-only)").input(
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                "path (optional)",
            )),
        ),
    ]
}

pub(super) fn available_session_modes() -> Vec<acp::SessionMode> {
    vec![
        acp::SessionMode::new("code", "Code").description("Write and edit code, execute tools"),
        acp::SessionMode::new("architect", "Architect")
            .description("Design and plan without writing code"),
        acp::SessionMode::new("ask", "Ask")
            .description("Answer questions without code changes or tools"),
    ]
}

pub(super) fn build_mode_state(current_mode_id: &acp::SessionModeId) -> acp::SessionModeState {
    acp::SessionModeState::new(current_mode_id.clone(), available_session_modes())
}

/// Build all session config options: model selector, thinking toggle, and auto-approve level.
///
/// `current_model` is the currently selected model key; empty string means use the first.
/// `thinking_enabled` and `auto_approve` reflect the current per-session values.
pub(super) fn build_config_options(
    available_models: &[String],
    current_model: &str,
    thinking_enabled: bool,
    auto_approve: &str,
) -> Vec<acp::SessionConfigOption> {
    let mut opts = Vec::new();

    if !available_models.is_empty() {
        let current_value = if current_model.is_empty() {
            available_models[0].clone()
        } else {
            current_model.to_owned()
        };
        let model_options: Vec<acp::SessionConfigSelectOption> = available_models
            .iter()
            .map(|m| acp::SessionConfigSelectOption::new(m.clone(), m.clone()))
            .collect();
        opts.push(
            acp::SessionConfigOption::select("model", "Model", current_value, model_options)
                .category(acp::SessionConfigOptionCategory::Model),
        );
    }

    let thinking_value = if thinking_enabled { "on" } else { "off" };
    opts.push(
        acp::SessionConfigOption::select(
            "thinking",
            "Extended Thinking",
            thinking_value.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("off".to_owned(), "Off".to_owned()),
                acp::SessionConfigSelectOption::new("on".to_owned(), "On".to_owned()),
            ],
        )
        .category(acp::SessionConfigOptionCategory::ThoughtLevel),
    );

    let approve_value = if ["suggest", "auto-edit", "full-auto"].contains(&auto_approve) {
        auto_approve.to_owned()
    } else {
        "suggest".to_owned()
    };
    opts.push(
        acp::SessionConfigOption::select(
            "auto_approve",
            "Auto-Approve",
            approve_value,
            vec![
                acp::SessionConfigSelectOption::new("suggest".to_owned(), "Suggest".to_owned()),
                acp::SessionConfigSelectOption::new("auto-edit".to_owned(), "Auto-Edit".to_owned()),
                acp::SessionConfigSelectOption::new("full-auto".to_owned(), "Full Auto".to_owned()),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Other(
            "behavior".to_owned(),
        )),
    );

    opts
}

fn tool_start_to_updates(data: zeph_core::ToolStartData) -> Vec<acp::SessionUpdate> {
    let tool_name = data.tool_name;
    let tool_call_id = data.tool_call_id;
    let params = data.params;
    let parent_tool_use_id = data.parent_tool_use_id;
    let started_at = data.started_at;
    // Derive a human-readable title from params when available.
    // For bash: use the command string (truncated). For others: fall back to tool_name.
    let title = params
        .as_ref()
        .and_then(|p| {
            p.get("command")
                .or_else(|| p.get("path"))
                .or_else(|| p.get("url"))
        })
        .and_then(|v| v.as_str())
        .map_or_else(
            || tool_name.clone(),
            |s| {
                const MAX_CHARS: usize = 120;
                if s.chars().count() > MAX_CHARS {
                    let truncated: String = s.chars().take(MAX_CHARS).collect();
                    format!("{truncated}…")
                } else {
                    s.to_owned()
                }
            },
        );
    let kind = tool_kind_from_name(&tool_name);
    let mut tool_call = acp::ToolCall::new(tool_call_id.clone(), title)
        .kind(kind)
        .status(acp::ToolCallStatus::InProgress);
    if let Some(ref p) = params
        && kind == acp::ToolKind::Read
        && let Some(loc) = p
            .get("file_path")
            .or_else(|| p.get("path"))
            .and_then(|v| v.as_str())
    {
        tool_call = tool_call.locations(vec![acp::ToolCallLocation::new(
            std::path::PathBuf::from(loc),
        )]);
    }
    if let Some(p) = params {
        tool_call = tool_call.raw_input(p);
    }
    // For execute-kind tools, register a display-only terminal keyed by tool_call_id.
    // This follows the Zed _meta extension pattern: terminal_info creates the terminal
    // widget in the ACP thread panel, terminal_output/terminal_exit populate it later.
    let mut meta = serde_json::Map::new();
    if kind == acp::ToolKind::Execute {
        meta.insert(
            "terminal_info".to_owned(),
            serde_json::json!({ "terminal_id": tool_call_id.clone() }),
        );
        tool_call = tool_call.content(vec![acp::ToolCallContent::Terminal(acp::Terminal::new(
            tool_call_id.clone(),
        ))]);
    }
    let mut claude_code = serde_json::Map::new();
    claude_code.insert(
        "toolName".to_owned(),
        serde_json::Value::String(tool_name.clone()),
    );
    // Record ISO 8601 start time so clients can compute elapsed duration.
    let started_at_iso = {
        let elapsed = started_at.elapsed();
        let now = std::time::SystemTime::now();
        let ts = now.checked_sub(elapsed).unwrap_or(now);
        chrono::DateTime::<chrono::Utc>::from(ts).to_rfc3339()
    };
    claude_code.insert(
        "startedAt".to_owned(),
        serde_json::Value::String(started_at_iso),
    );
    if let Some(parent_id) = parent_tool_use_id {
        claude_code.insert(
            "parentToolUseId".to_owned(),
            serde_json::Value::String(parent_id),
        );
    }
    meta.insert(
        "claudeCode".to_owned(),
        serde_json::Value::Object(claude_code),
    );
    tool_call = tool_call.meta(meta);
    vec![acp::SessionUpdate::ToolCall(tool_call)]
}

#[allow(clippy::too_many_arguments)]
fn terminal_tool_updates(
    tool_call_id: String,
    display: String,
    tool_name: String,
    elapsed_ms: Option<u64>,
    parent_tool_use_id: Option<String>,
    is_error: bool,
    status: acp::ToolCallStatus,
    acp_locations: Vec<acp::ToolCallLocation>,
) -> Vec<acp::SessionUpdate> {
    let mut output_meta = serde_json::Map::new();
    output_meta.insert(
        "terminal_output".to_owned(),
        serde_json::json!({ "terminal_id": tool_call_id, "data": display }),
    );
    let terminal_intermediate = acp::SessionUpdate::ToolCallUpdate(
        acp::ToolCallUpdate::new(tool_call_id.clone(), acp::ToolCallUpdateFields::new())
            .meta(output_meta),
    );
    let exit_code = u32::from(is_error);
    let mut exit_meta = serde_json::Map::new();
    exit_meta.insert(
        "terminal_exit".to_owned(),
        serde_json::json!({ "terminal_id": tool_call_id, "exit_code": exit_code, "signal": null }),
    );
    let mut cc = serde_json::Map::new();
    cc.insert("toolName".to_owned(), serde_json::Value::String(tool_name));
    if let Some(ms) = elapsed_ms {
        cc.insert("elapsedMs".to_owned(), serde_json::Value::Number(ms.into()));
    }
    if let Some(parent_id) = parent_tool_use_id {
        cc.insert(
            "parentToolUseId".to_owned(),
            serde_json::Value::String(parent_id),
        );
    }
    exit_meta.insert("claudeCode".to_owned(), serde_json::Value::Object(cc));
    let mut final_fields = acp::ToolCallUpdateFields::new()
        .status(status)
        .content(vec![acp::ToolCallContent::Terminal(acp::Terminal::new(
            tool_call_id.clone(),
        ))])
        .raw_output(serde_json::Value::String(display));
    if !acp_locations.is_empty() {
        final_fields = final_fields.locations(acp_locations);
    }
    let final_update = acp::SessionUpdate::ToolCallUpdate(
        acp::ToolCallUpdate::new(tool_call_id, final_fields).meta(exit_meta),
    );
    vec![terminal_intermediate, final_update]
}

#[allow(clippy::too_many_arguments)]
fn non_terminal_tool_updates(
    tool_call_id: String,
    display: String,
    diff: Option<zeph_core::DiffData>,
    tool_name: String,
    elapsed_ms: Option<u64>,
    parent_tool_use_id: Option<String>,
    status: acp::ToolCallStatus,
    acp_locations: Vec<acp::ToolCallLocation>,
) -> Vec<acp::SessionUpdate> {
    let mut content = vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
        acp::TextContent::new(display),
    ))];
    if let Some(d) = diff {
        let acp_diff = acp::Diff::new(std::path::PathBuf::from(&d.file_path), d.new_content)
            .old_text(d.old_content);
        content.push(acp::ToolCallContent::Diff(acp_diff));
    }
    let mut fields = acp::ToolCallUpdateFields::new()
        .status(status)
        .content(content);
    if !acp_locations.is_empty() {
        fields = fields.locations(acp_locations);
    }
    let mut meta = serde_json::Map::new();
    let mut cc = serde_json::Map::new();
    cc.insert("toolName".to_owned(), serde_json::Value::String(tool_name));
    if let Some(ms) = elapsed_ms {
        cc.insert("elapsedMs".to_owned(), serde_json::Value::Number(ms.into()));
    }
    if let Some(parent_id) = parent_tool_use_id {
        cc.insert(
            "parentToolUseId".to_owned(),
            serde_json::Value::String(parent_id),
        );
    }
    meta.insert("claudeCode".to_owned(), serde_json::Value::Object(cc));
    let update = acp::ToolCallUpdate::new(tool_call_id, fields).meta(meta);
    vec![acp::SessionUpdate::ToolCallUpdate(update)]
}

fn tool_output_to_updates(data: zeph_core::ToolOutputData) -> Vec<acp::SessionUpdate> {
    let tool_name = data.tool_name;
    let display = data.display;
    let diff = data.diff;
    let locations = data.locations;
    let tool_call_id = data.tool_call_id;
    let is_error = data.is_error;
    let terminal_id = data.terminal_id;
    let parent_tool_use_id = data.parent_tool_use_id;
    let raw_response = data.raw_response;
    let started_at = data.started_at;
    let elapsed_ms: Option<u64> =
        started_at.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
    let acp_locations: Vec<acp::ToolCallLocation> = locations
        .unwrap_or_default()
        .into_iter()
        .map(|p| acp::ToolCallLocation::new(std::path::PathBuf::from(p)))
        .collect();

    let status = if is_error {
        acp::ToolCallStatus::Failed
    } else {
        acp::ToolCallStatus::Completed
    };

    // Build intermediate tool_call_update with toolResponse when raw_response is present.
    // This update has no status — it only carries the structured response payload.
    let response_update = raw_response.map(|resp| {
        let mut resp_meta = serde_json::Map::new();
        let mut cc = serde_json::Map::new();
        cc.insert(
            "toolName".to_owned(),
            serde_json::Value::String(tool_name.clone()),
        );
        cc.insert("toolResponse".to_owned(), resp);
        if let Some(ref parent_id) = parent_tool_use_id {
            cc.insert(
                "parentToolUseId".to_owned(),
                serde_json::Value::String(parent_id.clone()),
            );
        }
        resp_meta.insert("claudeCode".to_owned(), serde_json::Value::Object(cc));
        acp::SessionUpdate::ToolCallUpdate(
            acp::ToolCallUpdate::new(tool_call_id.clone(), acp::ToolCallUpdateFields::new())
                .meta(resp_meta),
        )
    });

    let final_updates = if terminal_id.is_some() {
        terminal_tool_updates(
            tool_call_id,
            display,
            tool_name,
            elapsed_ms,
            parent_tool_use_id,
            is_error,
            status,
            acp_locations,
        )
    } else {
        non_terminal_tool_updates(
            tool_call_id,
            display,
            diff,
            tool_name,
            elapsed_ms,
            parent_tool_use_id,
            status,
            acp_locations,
        )
    };

    let mut result = Vec::with_capacity(final_updates.len() + 1);
    if let Some(ru) = response_update {
        result.push(ru);
    }
    result.extend(final_updates);
    result
}

pub(super) fn loopback_event_to_updates(event: LoopbackEvent) -> Vec<acp::SessionUpdate> {
    match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text)
            if text.is_empty() || is_tool_use_marker(&text) =>
        {
            vec![]
        }
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => {
            if text.is_empty() {
                vec![]
            } else {
                vec![acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(text.into()),
                )]
            }
        }
        LoopbackEvent::Status(text) if text.is_empty() => vec![],
        LoopbackEvent::Status(text) => vec![
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new("\n".into())),
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(text.into())),
        ],
        LoopbackEvent::ToolStart(data) => tool_start_to_updates(*data),
        LoopbackEvent::ToolOutput(data) => tool_output_to_updates(*data),
        LoopbackEvent::Flush => vec![],
        #[cfg(feature = "unstable-session-usage")]
        LoopbackEvent::Usage {
            input_tokens,
            output_tokens,
            context_window,
        } => {
            let used = input_tokens.saturating_add(output_tokens);
            vec![acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(
                used,
                context_window,
            ))]
        }
        #[cfg(not(feature = "unstable-session-usage"))]
        LoopbackEvent::Usage { .. } => vec![],
        #[cfg(feature = "unstable-session-info-update")]
        LoopbackEvent::SessionTitle(title) => {
            vec![acp::SessionUpdate::SessionInfoUpdate(
                acp::SessionInfoUpdate::new().title(title),
            )]
        }
        #[cfg(not(feature = "unstable-session-info-update"))]
        LoopbackEvent::SessionTitle(_) => vec![],
        LoopbackEvent::Plan(entries) => {
            let acp_entries = entries
                .into_iter()
                .map(|(content, status)| {
                    let acp_status = match status {
                        zeph_core::channel::PlanItemStatus::Pending => {
                            acp::PlanEntryStatus::Pending
                        }
                        zeph_core::channel::PlanItemStatus::InProgress => {
                            acp::PlanEntryStatus::InProgress
                        }
                        zeph_core::channel::PlanItemStatus::Completed => {
                            acp::PlanEntryStatus::Completed
                        }
                    };
                    acp::PlanEntry::new(content, acp::PlanEntryPriority::Medium, acp_status)
                })
                .collect();
            vec![acp::SessionUpdate::Plan(acp::Plan::new(acp_entries))]
        }
        LoopbackEvent::ThinkingChunk(text) if text.is_empty() => vec![],
        LoopbackEvent::ThinkingChunk(text) => {
            vec![acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(text.into()),
            )]
        }
        // Stop hints are consumed directly in the prompt() loop and must not reach here.
        LoopbackEvent::Stop(_) => vec![],
    }
}
