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
use crate::CacheTtl;

/// Raw message split, with no no-prefill gating applied.
///
/// Not re-exported at `claude` module top level and not meant to be called directly from a
/// request-construction path — `ClaudeProvider::plain_history` is the funnel that applies the
/// no-prefill strip after this split (see its doc comment for why).
///
/// Visibility is `pub(in crate::claude)` rather than plain `pub(super)`, but in the current
/// module nesting (`request` is a direct child of `claude`, and `mod.rs` *is* the `claude`
/// module) the two are functionally identical — this is a self-documenting anchor to the
/// intended boundary, NOT a new compile-time bypass barrier. Rust has no way to grant a child
/// module's item to its parent while excluding the parent's other children, so `claude::tests`
/// remains exactly as reachable as before in raw visibility terms. A true compile-error barrier
/// is architecturally impossible while `structured_history`/`plain_history` live in the parent
/// module (`claude`/`mod.rs`) and therefore need at least this visibility to call these
/// functions — a new request-construction path added directly in `mod.rs` that calls
/// `split_messages`/`split_messages_structured` instead of the funnel would still compile fine;
/// it remains a code-review catch, not a compiler error. The actual hardening this change
/// delivers is moving the tests that used to call this directly out of `claude::tests` and into
/// this file's own `mod tests` below — reduced discoverability/habit, not a compiler-enforced
/// guarantee. A genuine compile-time guarantee (e.g. a newtype only constructible by
/// `structured_history`/`plain_history`, so a request body cannot accept an ungated history) is
/// tracked as a follow-up: #6158.
pub(in crate::claude) fn split_messages(
    messages: &[Message],
) -> (Option<String>, Vec<ApiMessage<'_>>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages {
        if !msg.metadata.visibility.is_agent_visible() {
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

/// Raw structured message split, with no no-prefill gating applied.
///
/// Not re-exported at `claude` module top level and not meant to be called directly from a
/// request-construction path — `ClaudeProvider::structured_history` is the funnel that applies
/// the no-prefill strip after this split (see its doc comment for why). See [`split_messages`]
/// for why the `pub(in crate::claude)` visibility here is a documentation anchor rather than a
/// new compile-time barrier, and for the compile-time-enforcement follow-up (#6158).
pub(in crate::claude) fn split_messages_structured(
    messages: &[Message],
    cache_user_messages: bool,
    ttl: Option<CacheTtl>,
) -> (Option<String>, Vec<StructuredApiMessage>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages
        .iter()
        .filter(|m| m.metadata.visibility.is_agent_visible() && m.role == Role::System)
    {
        system_parts.push(msg.to_llm_content());
    }

    // Collect only agent-visible non-system messages so that idx-based peek always lands on a
    // user or assistant message (RC4: system messages in `visible` would break +1 index peek).
    let visible: Vec<&Message> = messages
        .iter()
        .filter(|m| m.metadata.visibility.is_agent_visible() && m.role != Role::System)
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
        apply_cache_breakpoint(&mut chat, ttl);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ImageData, MessageMetadata};

    #[test]
    fn split_messages_extracts_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "You are helpful.");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
    }

    #[test]
    fn split_messages_no_system() {
        let messages = vec![Message {
            role: Role::User,
            content: "Hi".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn split_messages_multiple_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "Part 1".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::System,
                content: "Part 2".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let (system, _) = split_messages(&messages);
        assert_eq!(system.unwrap(), "Part 1\n\nPart 2");
    }

    #[test]
    fn split_messages_all_roles() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "system prompt".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "user msg".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "followup".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "system prompt");
        assert_eq!(chat.len(), 3);
        assert_eq!(chat[0].role, "user");
        assert_eq!(chat[0].content, "user msg");
        assert_eq!(chat[1].role, "assistant");
        assert_eq!(chat[1].content, "assistant reply");
        assert_eq!(chat[2].role, "user");
        assert_eq!(chat[2].content, "followup");
    }

    #[test]
    fn split_messages_empty() {
        let (system, chat) = split_messages(&[]);
        assert!(system.is_none());
        assert!(chat.is_empty());
    }

    #[test]
    fn split_messages_only_system() {
        let messages = vec![Message {
            role: Role::System,
            content: "instruction".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "instruction");
        assert!(chat.is_empty());
    }

    #[test]
    fn split_messages_only_assistant() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: "reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "assistant");
    }

    #[test]
    fn split_messages_interleaved_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "first".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "question".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::System,
                content: "second".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "first\n\nsecond");
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn split_messages_structured_with_tool_parts() {
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "I'll run that".into(),
                    },
                    MessagePart::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file1.rs".into(),
                    is_error: false,
                }],
            ),
        ];
        let (system, chat) = split_messages_structured(&messages, true, None);
        assert!(system.is_none());
        assert_eq!(chat.len(), 2);

        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(assistant_json.contains("tool_use"));
        assert!(assistant_json.contains("\"id\":\"t1\""));

        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(user_json.contains("tool_result"));
        assert!(user_json.contains("\"tool_use_id\":\"t1\""));
    }

    /// FIX2 regression: an assistant message with a `ToolUse` part that has NO matching
    /// `ToolResult` in the next user message must emit a text block instead of a `tool_use`
    /// block, preventing Claude API 400 errors caused by unmatched `tool_use/tool_result` pairs.
    #[test]
    fn split_messages_structured_downgrades_unmatched_tool_use_to_text() {
        // Orphaned assistant[ToolUse] — no following user[ToolResult].
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "Let me run this.".into(),
                    },
                    MessagePart::ToolUse {
                        id: "orphan_id".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            ),
            // Next message is NOT a ToolResult response — simulates compaction-split orphan.
            Message::from_parts(
                Role::User,
                vec![MessagePart::Text {
                    text: "Thanks, what did you find?".into(),
                }],
            ),
        ];

        let (_, chat) = split_messages_structured(&messages, false, None);
        assert_eq!(chat.len(), 2);

        // The assistant block must NOT contain a tool_use block for the unmatched ID.
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            !assistant_json.contains("\"type\":\"tool_use\""),
            "unmatched tool_use must be downgraded: {assistant_json}"
        );
        // The orphaned ID must appear in a text fallback instead.
        assert!(
            assistant_json.contains("orphan_id") || assistant_json.contains("shell"),
            "downgraded tool_use must appear as text fallback: {assistant_json}"
        );
    }

    /// FIX2 regression: a matched `tool_use/tool_result` pair must still emit a real
    /// `tool_use` block. The defensive check must not break valid exchanges.
    #[test]
    fn split_messages_structured_preserves_matched_tool_use_block() {
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "matched_id".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "matched_id".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
            ),
        ];

        let (_, chat) = split_messages_structured(&messages, false, None);
        assert_eq!(chat.len(), 2);

        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "matched tool_use must be emitted as tool_use block: {assistant_json}"
        );
        assert!(assistant_json.contains("\"id\":\"matched_id\""));
    }

    /// RC1 regression: when a `ToolUse` was downgraded to text (because the next user message had
    /// no matching `ToolResult`), the corresponding `ToolResult` in the user message must ALSO be
    /// downgraded to text instead of being emitted as a native `ToolResult` block.
    /// Previously only the `ToolUse` was downgraded, leaving an orphaned `ToolResult` that caused
    /// Claude API 400 errors on session restore.
    #[test]
    fn split_structured_downgrades_orphaned_tool_result() {
        // Scenario: assistant emits tool_use "t_orphan", but the following user message has a
        // ToolResult for a DIFFERENT id — so "t_orphan" is downgraded. The ToolResult for
        // "t_orphan" (which does appear in the user message) must also be downgraded.
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "t_orphan".into(),
                    name: "memory_save".into(),
                    input: serde_json::json!({"content": "x"}),
                }],
            ),
            // User message references t_orphan but the assistant ToolUse was not matched
            // (there is no ToolResult for t_orphan in the NEXT user message from assistant's
            // perspective — the assistant sees this user message has t_orphan, but the
            // matched_tool_ids logic checks whether the ToolResult id matches).
            // To trigger the orphan path: provide a user message whose ToolResult id does NOT
            // match the ToolUse id — so matched_tool_ids for "t_orphan" is empty.
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_orphan".into(),
                    content: "saved".into(),
                    is_error: false,
                }],
            ),
        ];

        // Verify the full round-trip: the assistant ToolUse is matched (t_orphan has a
        // corresponding ToolResult), so this tests the happy path.
        let (_, chat) = split_messages_structured(&messages, false, None);
        assert_eq!(chat.len(), 2);

        // The assistant message must emit t_orphan as a real tool_use (matched pair).
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "matched tool_use must be emitted as native block: {assistant_json}"
        );

        // The user message must emit t_orphan as a real tool_result (matched pair).
        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(
            user_json.contains("\"type\":\"tool_result\""),
            "matched tool_result must be emitted as native block: {user_json}"
        );

        // Now test the actual RC1 scenario: assistant emits TWO tool_use IDs but the user
        // message only has a ToolResult for ONE of them. The unmatched tool_use is downgraded,
        // and the ToolResult for the unmatched id must NOT appear in the user message output.
        let messages_partial = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::ToolUse {
                        id: "t_matched".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                    MessagePart::ToolUse {
                        id: "t_missing_result".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "pwd"}),
                    },
                ],
            ),
            // User only provides result for t_matched; t_missing_result has no ToolResult.
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_matched".into(),
                    content: "output".into(),
                    is_error: false,
                }],
            ),
        ];

        let (_, chat2) = split_messages_structured(&messages_partial, false, None);
        assert_eq!(chat2.len(), 2);

        // t_missing_result must be downgraded to text in the assistant message: if its ID
        // appears at all it must not be inside a native tool_use block.
        let assistant_json2 = serde_json::to_string(&chat2[0]).unwrap();
        let has_native_missing = assistant_json2.contains("\"type\":\"tool_use\"")
            && assistant_json2.contains("\"id\":\"t_missing_result\"");
        assert!(
            !has_native_missing,
            "t_missing_result must not appear as a native tool_use block: {assistant_json2}"
        );

        // t_matched must still be emitted as a real tool_use.
        assert!(
            assistant_json2.contains("\"id\":\"t_matched\""),
            "t_matched must be emitted as native tool_use: {assistant_json2}"
        );

        // The user message must only have t_matched as a real tool_result.
        let user_json2 = serde_json::to_string(&chat2[1]).unwrap();
        assert!(
            user_json2.contains("\"type\":\"tool_result\""),
            "matched tool_result must be emitted as native block: {user_json2}"
        );
        assert!(
            user_json2.contains("\"tool_use_id\":\"t_matched\""),
            "t_matched tool_result must be present: {user_json2}"
        );
    }

    /// RC4 regression: system messages interleaved in the message list must NOT appear in the
    /// `visible` index array used by `split_messages_structured`. If they did, the +1 peek used
    /// to check whether a `ToolUse` has a matching `ToolResult` would land on a system message
    /// instead of the actual next user message, causing false-positive downgrades.
    #[test]
    fn split_structured_system_not_in_visible() {
        // System message appears between the assistant ToolUse and the user ToolResult.
        // With the RC4 fix the system message is filtered out of `visible`, so idx+1 correctly
        // lands on the user message and the ToolUse is NOT downgraded.
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are a helpful assistant.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "t_sys_test".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
            ),
            // Interleaved system message — must not disrupt the +1 peek.
            Message {
                role: Role::System,
                content: "Additional context injected mid-conversation.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_sys_test".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
            ),
        ];

        let (system_text, chat) = split_messages_structured(&messages, false, None);

        // Both system messages must be extracted to the system string.
        let system = system_text.unwrap_or_default();
        assert!(
            system.contains("You are a helpful assistant."),
            "first system message must be in system text: {system}"
        );
        assert!(
            system.contains("Additional context"),
            "interleaved system message must be in system text: {system}"
        );

        // chat must contain only user and assistant messages (no system).
        assert_eq!(
            chat.len(),
            2,
            "chat must contain exactly assistant + user messages (no system), got {}",
            chat.len()
        );
        assert_eq!(chat[0].role, "assistant");
        assert_eq!(chat[1].role, "user");

        // The ToolUse must NOT be downgraded — system messages must not break the +1 peek.
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "ToolUse must be emitted as native block when system messages are filtered: {assistant_json}"
        );
        assert!(
            assistant_json.contains("\"id\":\"t_sys_test\""),
            "correct tool_use id must be present: {assistant_json}"
        );

        // The ToolResult must be emitted as a native block (not downgraded).
        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(
            user_json.contains("\"type\":\"tool_result\""),
            "ToolResult must be emitted as native block: {user_json}"
        );
    }

    #[test]
    fn split_messages_structured_produces_image_block() {
        let data = vec![0xFFu8, 0xD8, 0xFF];
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "look at this".into(),
                },
                MessagePart::Image(Box::new(ImageData {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                })),
            ],
        );
        let (system, chat) = split_messages_structured(&[msg], true, None);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
        match &chat[0].content {
            StructuredContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    AnthropicContentBlock::Text { text, .. } => assert_eq!(text, "look at this"),
                    _ => panic!("expected Text block first"),
                }
                match &blocks[1] {
                    AnthropicContentBlock::Image { source } => {
                        assert_eq!(source.source_type, "base64");
                        assert_eq!(source.media_type, "image/jpeg");
                        assert_eq!(source.data, STANDARD.encode(&data));
                    }
                    _ => panic!("expected Image block second"),
                }
            }
            StructuredContent::Text(_) => panic!("expected Blocks content"),
        }
    }

    #[test]
    fn thinking_block_serializes_in_structured_message() {
        let msg = Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ThinkingBlock {
                    thinking: "my reasoning".into(),
                    signature: "abc".into(),
                },
                MessagePart::Text {
                    text: "answer".into(),
                },
            ],
        );
        let (_, chat) = split_messages_structured(&[msg], true, None);
        assert_eq!(chat.len(), 1);
        let json = serde_json::to_value(&chat[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "my reasoning");
        assert_eq!(blocks[0]["signature"], "abc");
        assert_eq!(blocks[1]["type"], "text");
    }

    #[test]
    fn redacted_thinking_block_serializes_in_structured_message() {
        let msg = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::RedactedThinkingBlock {
                data: "secret".into(),
            }],
        );
        let (_, chat) = split_messages_structured(&[msg], true, None);
        let json = serde_json::to_value(&chat[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "redacted_thinking");
        assert_eq!(blocks[0]["data"], "secret");
    }

    #[test]
    fn split_messages_structured_single_message_no_cache_breakpoint() {
        let messages = vec![Message {
            role: Role::User,
            content: "only message".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (_, chat) = split_messages_structured(&messages, true, None);
        assert_eq!(chat.len(), 1);
        // With only 1 message, no breakpoint is placed
        let json = serde_json::to_value(&chat[0]).unwrap();
        let has_cache = json.to_string().contains("cache_control");
        assert!(
            !has_cache,
            "single message must not have cache_control breakpoint"
        );
    }

    #[test]
    fn split_messages_structured_two_messages_places_breakpoint_on_user() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "first user".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (_, chat) = split_messages_structured(&messages, true, None);
        assert_eq!(chat.len(), 2);
        // Breakpoint must be on the user message at index 0 (only user in range)
        let user_json = serde_json::to_value(&chat[0]).unwrap();
        assert!(
            user_json.to_string().contains("cache_control"),
            "user message must carry cache_control breakpoint"
        );
        let assistant_json = serde_json::to_value(&chat[1]).unwrap();
        assert!(
            !assistant_json.to_string().contains("cache_control"),
            "assistant message must not have cache_control"
        );
    }

    #[test]
    fn split_messages_structured_breakpoint_targets_last_minus_20_position() {
        // Build 25 messages: user/assistant alternating, user first
        let mut messages = Vec::new();
        for i in 0..25u32 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            let content = format!("message {i}");
            messages.push(Message {
                role,
                content,
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        let (_, chat) = split_messages_structured(&messages, true, None);
        assert_eq!(chat.len(), 25);
        // target = 25 - 20 = 5; first user at or after index 5 is index 6 (even indices are user)
        // Actually index 5 is assistant (odd), so search finds index 6 (user)
        let mut breakpoint_idx = None;
        for (i, msg) in chat.iter().enumerate() {
            let json = serde_json::to_value(msg).unwrap();
            if json.to_string().contains("cache_control") {
                breakpoint_idx = Some(i);
                break;
            }
        }
        let idx = breakpoint_idx.expect("must have a breakpoint somewhere");
        assert_eq!(
            chat[idx].role, "user",
            "breakpoint must be on a user message"
        );
        // Breakpoint index must be >= max(0, total-20) = 5
        assert!(idx >= 5, "breakpoint must be at or after position total-20");
    }

    #[test]
    fn split_messages_structured_cache_enabled_adds_cache_control() {
        let messages = vec![
            Message::from_legacy(Role::User, "first"),
            Message::from_legacy(Role::Assistant, "answer"),
            Message::from_legacy(Role::User, "second"),
        ];
        let (_, chat) = split_messages_structured(&messages, true, None);
        assert_eq!(chat.len(), 3);
        // Breakpoint targets the user message at max(0, total-20) = 0, which is chat[0].
        let has_cache = chat.iter().any(|m| {
            m.role == "user"
                && match &m.content {
                    StructuredContent::Blocks(blocks) => blocks.iter().any(|b| {
                        matches!(
                            b,
                            AnthropicContentBlock::Text {
                                cache_control: Some(_),
                                ..
                            }
                        )
                    }),
                    StructuredContent::Text(_) => false,
                }
        });
        assert!(
            has_cache,
            "at least one user message must have cache_control when enabled"
        );
    }

    #[test]
    fn split_messages_structured_cache_disabled_no_cache_control() {
        let messages = vec![
            Message::from_legacy(Role::User, "first"),
            Message::from_legacy(Role::Assistant, "answer"),
            Message::from_legacy(Role::User, "second"),
        ];
        let (_, chat) = split_messages_structured(&messages, false, None);
        assert_eq!(chat.len(), 3);
        // With cache disabled, last user message stays as plain Text.
        assert!(
            matches!(&chat[2].content, StructuredContent::Text(_)),
            "last user message must remain Text when cache disabled"
        );
    }

    #[test]
    fn split_messages_structured_compaction_round_trip() {
        // Compaction in an assistant message must be emitted verbatim as an
        // AnthropicContentBlock::Compaction so the API can prune history correctly.
        // A Compaction in a user message must be silently dropped.
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "Before compaction.".into(),
                    },
                    MessagePart::Compaction {
                        summary: "History was compacted here.".into(),
                    },
                ],
            ),
            Message::from_parts(
                Role::User,
                vec![
                    MessagePart::Text {
                        text: "Continue.".into(),
                    },
                    MessagePart::Compaction {
                        summary: "should be dropped".into(),
                    },
                ],
            ),
        ];
        let (system, chat) = split_messages_structured(&messages, false, None);
        assert!(system.is_none());
        assert_eq!(chat.len(), 2);

        // Assistant message: must contain a Compaction block with the original summary.
        if let StructuredContent::Blocks(blocks) = &chat[0].content {
            let has_compaction = blocks.iter().any(|b| {
                matches!(b, AnthropicContentBlock::Compaction { summary }
                    if summary == "History was compacted here.")
            });
            assert!(
                has_compaction,
                "assistant Compaction block must be preserved"
            );
        } else {
            panic!("expected Blocks for assistant message");
        }

        // User message: Compaction must be silently dropped.
        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(
            !user_json.contains("compaction"),
            "Compaction in user message must be dropped"
        );
    }
}
