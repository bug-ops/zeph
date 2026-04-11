// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Message conversion and request building utilities for the Claude provider.

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::provider::{ChatResponse, Message, MessagePart, Role, ThinkingBlock, ToolUseRequest};

use super::cache::apply_cache_breakpoint;
use super::types::{
    AnthropicContentBlock, ApiMessage, ImageSource, StructuredApiMessage, StructuredContent,
    ToolApiResponse,
};

pub(super) fn split_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage<'_>>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages {
        if !msg.metadata.agent_visible {
            continue;
        }
        match msg.role {
            Role::System => system_parts.push(msg.to_llm_content()),
            Role::User | Role::Assistant => {
                let content = msg.to_llm_content();
                if !content.trim().is_empty() {
                    let role = if msg.role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    };
                    chat.push(ApiMessage { role, content });
                }
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

pub(super) fn split_messages_structured(
    messages: &[Message],
    cache_user_messages: bool,
) -> (Option<String>, Vec<StructuredApiMessage>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages
        .iter()
        .filter(|m| m.metadata.agent_visible && m.role == Role::System)
    {
        system_parts.push(msg.to_llm_content());
    }

    // Collect only agent-visible non-system messages so that idx-based peek always lands on a
    // user or assistant message (RC4: system messages in `visible` would break +1 index peek).
    let visible: Vec<&Message> = messages
        .iter()
        .filter(|m| m.metadata.agent_visible && m.role != Role::System)
        .collect();

    // Track which tool_use IDs were actually emitted as native AnthropicContentBlock::ToolUse
    // by the most recent assistant message. When processing the following user message, any
    // ToolResult block whose tool_use_id is not in this set is downgraded to text — prevents
    // API 400 caused by orphaned ToolResult referencing a non-existent tool_use (RC1 fix).
    let mut last_emitted_tool_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (idx, msg) in visible.iter().enumerate() {
        match msg.role {
            Role::System => {} // already extracted above
            Role::User | Role::Assistant => {
                let role = if msg.role == Role::User {
                    "user"
                } else {
                    "assistant"
                };
                let has_structured_parts = msg.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::ToolUse { .. }
                            | MessagePart::ToolResult { .. }
                            | MessagePart::Image(_)
                            | MessagePart::ThinkingBlock { .. }
                            | MessagePart::RedactedThinkingBlock { .. }
                            | MessagePart::Compaction { .. }
                    )
                });

                if has_structured_parts {
                    let is_assistant = msg.role == Role::Assistant;
                    // For assistant messages, pre-compute which tool_use IDs are matched by
                    // the next visible user message. Unmatched IDs are downgraded to text to
                    // prevent Claude API 400 (tool_use without tool_result).
                    let matched_tool_ids = if is_assistant {
                        Some(compute_matched_tool_ids(msg, visible.get(idx + 1)))
                    } else {
                        None
                    };
                    // Reset emitted tool IDs at the start of each assistant message so user
                    // messages can check against the immediately preceding assistant only.
                    if is_assistant {
                        last_emitted_tool_ids.clear();
                    }
                    let blocks = convert_parts_to_blocks(
                        &msg.parts,
                        is_assistant,
                        matched_tool_ids.as_ref(),
                        &mut last_emitted_tool_ids,
                    );
                    chat.push(StructuredApiMessage {
                        role: role.to_owned(),
                        content: StructuredContent::Blocks(blocks),
                    });
                } else {
                    // Non-structured user/assistant message: clear emitted tool IDs since
                    // no tool pairs are possible across a plain text message boundary.
                    if msg.role == Role::Assistant {
                        last_emitted_tool_ids.clear();
                    }
                    let text = msg.to_llm_content();
                    if !text.trim().is_empty() {
                        chat.push(StructuredApiMessage {
                            role: role.to_owned(),
                            content: StructuredContent::Text(text.to_owned()),
                        });
                    }
                }
            }
        }
    }

    // Place 1 message-level cache breakpoint at the user message closest to position
    // (total - 20) to maximize the 20-block lookback window coverage.
    if cache_user_messages && chat.len() > 1 {
        apply_cache_breakpoint(&mut chat);
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

pub(super) fn parse_tool_response(resp: ToolApiResponse) -> (ChatResponse, Option<String>) {
    let truncated = resp.stop_reason.as_deref() == Some("max_tokens");
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut thinking_blocks = Vec::new();
    let mut compaction_summary: Option<String> = None;

    for block in resp.content {
        match block {
            AnthropicContentBlock::Text { text, .. } => text_parts.push(text),
            AnthropicContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolUseRequest {
                    id,
                    name: name.into(),
                    input,
                });
            }
            AnthropicContentBlock::Thinking {
                thinking,
                signature,
            } => {
                tracing::debug!(len = thinking.len(), "Claude thinking block received");
                thinking_blocks.push(ThinkingBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AnthropicContentBlock::RedactedThinking { data } => {
                tracing::debug!("Claude redacted_thinking block received");
                thinking_blocks.push(ThinkingBlock::Redacted { data });
            }
            AnthropicContentBlock::Compaction { summary } => {
                tracing::info!(
                    summary_len = summary.len(),
                    "Claude server-side compaction block received"
                );
                compaction_summary = Some(summary);
            }
            AnthropicContentBlock::ToolResult { .. } | AnthropicContentBlock::Image { .. } => {}
        }
    }

    // When response was cut off by max_tokens with pending tool calls, the tool
    // inputs are incomplete JSON. Discard them and surface the partial text so
    // the agent loop can retry rather than executing a malformed tool call.
    if truncated && !tool_calls.is_empty() {
        tracing::warn!(
            tool_count = tool_calls.len(),
            "response truncated by max_tokens with pending tool calls; discarding incomplete tool use"
        );
        let combined = text_parts.join("");
        return (
            ChatResponse::Text(if combined.is_empty() {
                "[Response truncated: max_tokens limit reached. Please reduce the request scope.]"
                    .to_owned()
            } else {
                combined
            }),
            compaction_summary,
        );
    }

    let response = if tool_calls.is_empty() {
        let combined = text_parts.join("");
        // Inject the truncation marker so the agent loop can emit StopReason::MaxTokens.
        let text = if truncated {
            let marker = crate::provider::MAX_TOKENS_TRUNCATION_MARKER;
            if combined.is_empty() {
                format!("[Response truncated: {marker}. Please reduce the request scope.]")
            } else {
                format!("{combined}\n[Response truncated: {marker}.]")
            }
        } else {
            combined
        };
        ChatResponse::Text(text)
    } else {
        let text = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks,
        }
    };
    (response, compaction_summary)
}

fn push_tool_use_block(
    blocks: &mut Vec<AnthropicContentBlock>,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    matched_tool_ids: Option<&std::collections::HashSet<&str>>,
    last_emitted_tool_ids: &mut std::collections::HashSet<String>,
) {
    let matched = matched_tool_ids.is_some_and(|ids| ids.contains(id));
    if matched {
        last_emitted_tool_ids.insert(id.to_owned());
        blocks.push(AnthropicContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: input.clone(),
        });
    } else {
        tracing::warn!(
            tool_use_id = %id,
            tool_name = %name,
            "downgrading unmatched tool_use to text in API request"
        );
        blocks.push(AnthropicContentBlock::Text {
            text: format!("[tool_use: {name}] {input}"),
            cache_control: None,
        });
    }
}

fn push_tool_result_block(
    blocks: &mut Vec<AnthropicContentBlock>,
    tool_use_id: &str,
    content: &str,
    is_error: bool,
    last_emitted_tool_ids: &std::collections::HashSet<String>,
) {
    if last_emitted_tool_ids.contains(tool_use_id) {
        blocks.push(AnthropicContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: content.to_owned(),
            is_error,
            cache_control: None,
        });
    } else {
        tracing::warn!(
            tool_use_id = %tool_use_id,
            "downgrading orphaned tool_result to text in API request"
        );
        if !content.trim().is_empty() {
            blocks.push(AnthropicContentBlock::Text {
                text: content.to_owned(),
                cache_control: None,
            });
        }
    }
}

/// Convert message parts into `AnthropicContentBlock`s, respecting tool-use/result pairing rules.
///
/// - `is_assistant`: whether the message is from the assistant role
/// - `matched_tool_ids`: set of `tool_use` IDs that are matched by the next user message
/// - `last_emitted_tool_ids`: tracks IDs emitted as native `ToolUse` to detect orphaned results
pub(super) fn convert_parts_to_blocks(
    parts: &[MessagePart],
    is_assistant: bool,
    matched_tool_ids: Option<&std::collections::HashSet<&str>>,
    last_emitted_tool_ids: &mut std::collections::HashSet<String>,
) -> Vec<AnthropicContentBlock> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            MessagePart::Text { text }
            | MessagePart::Recall { text }
            | MessagePart::CodeContext { text }
            | MessagePart::Summary { text }
            | MessagePart::CrossSession { text } => {
                if !text.trim().is_empty() {
                    blocks.push(AnthropicContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    });
                }
            }
            MessagePart::ToolOutput {
                tool_name, body, ..
            } => {
                blocks.push(AnthropicContentBlock::Text {
                    text: format!("[tool output: {tool_name}]\n{body}"),
                    cache_control: None,
                });
            }
            MessagePart::ToolUse { id, name, input } if is_assistant => {
                // Downgrade to text if the tool_use ID is not matched by the
                // next user message — prevents API 400 on orphaned tool_use.
                push_tool_use_block(
                    &mut blocks,
                    id,
                    name,
                    input,
                    matched_tool_ids,
                    last_emitted_tool_ids,
                );
            }
            MessagePart::ToolUse { name, input, .. } => {
                blocks.push(AnthropicContentBlock::Text {
                    text: format!("[tool_use: {name}] {input}"),
                    cache_control: None,
                });
            }
            MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if !is_assistant => {
                // Downgrade to text if the tool_use_id was not emitted as a
                // native ToolUse by the preceding assistant message (RC1 fix).
                push_tool_result_block(
                    &mut blocks,
                    tool_use_id,
                    content,
                    *is_error,
                    last_emitted_tool_ids,
                );
            }
            MessagePart::ToolResult { content, .. } => {
                if !content.trim().is_empty() {
                    blocks.push(AnthropicContentBlock::Text {
                        text: content.clone(),
                        cache_control: None,
                    });
                }
            }
            MessagePart::Image(img) => {
                blocks.push(AnthropicContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_owned(),
                        media_type: img.mime_type.clone(),
                        data: STANDARD.encode(&img.data),
                    },
                });
            }
            MessagePart::ThinkingBlock {
                thinking,
                signature,
            } if is_assistant => {
                blocks.push(AnthropicContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                });
            }
            MessagePart::RedactedThinkingBlock { data } if is_assistant => {
                blocks.push(AnthropicContentBlock::RedactedThinking { data: data.clone() });
            }
            // Compaction blocks must be sent back verbatim in subsequent turns
            // so the Claude API can prune prior history correctly.
            MessagePart::Compaction { summary } if is_assistant => {
                blocks.push(AnthropicContentBlock::Compaction {
                    summary: summary.clone(),
                });
            }
            // Compaction blocks in user messages and thinking blocks are silently dropped.
            MessagePart::Compaction { .. }
            | MessagePart::ThinkingBlock { .. }
            | MessagePart::RedactedThinkingBlock { .. } => {}
        }
    }
    blocks
}

pub(super) fn compute_matched_tool_ids<'m>(
    msg: &'m Message,
    next: Option<&&'m Message>,
) -> std::collections::HashSet<&'m str> {
    msg.parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolUse { id, .. } = p {
                Some(id.as_str())
            } else {
                None
            }
        })
        .filter(|uid| {
            next.is_some_and(|next_msg| {
                next_msg.role == Role::User
                    && next_msg.parts.iter().any(|np| {
                        matches!(
                            np,
                            MessagePart::ToolResult { tool_use_id, .. }
                                if tool_use_id.as_str() == *uid
                        )
                    })
            })
        })
        .collect()
}
