// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde_json::json;
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::agent::{estimate_parts_size, trim_parent_messages};

fn text_msg(role: Role, text: &str) -> Message {
    Message::from_parts(
        role,
        vec![MessagePart::Text {
            text: text.to_owned(),
        }],
    )
}

fn tool_use_msg(id: &str, name: &str) -> Message {
    Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: json!({}),
        }],
    )
}

fn tool_result_msg(tool_use_id: &str, content: &str) -> Message {
    Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        }],
    )
}

#[test]
fn trim_parent_messages_drops_orphaned_tool_results() {
    // Slice starts with a user ToolResult for "tu_A", but the corresponding ToolUse is NOT
    // in the slice (it was truncated away).  The orphan must be removed.
    let mut msgs = vec![
        tool_result_msg("tu_A", "result-a"),
        text_msg(Role::Assistant, "ok"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    // The orphaned ToolResult message must be gone; only the text assistant message remains.
    assert_eq!(msgs.len(), 1, "orphaned ToolResult message must be removed");
    assert!(
        msgs[0]
            .parts
            .iter()
            .all(|p| !matches!(p, MessagePart::ToolResult { .. })),
        "no ToolResult parts must remain"
    );
}

#[test]
fn trim_parent_messages_keeps_matched_tool_pairs() {
    // Both ToolUse and ToolResult for "tu_B" are in the slice — both must be kept.
    let mut msgs = vec![
        tool_use_msg("tu_B", "shell"),
        tool_result_msg("tu_B", "output-b"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(msgs.len(), 2, "matched pair must not be removed");
    let has_use = msgs[0]
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "tu_B"));
    let has_result = msgs[1]
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "tu_B"));
    assert!(has_use, "ToolUse must be preserved");
    assert!(has_result, "ToolResult must be preserved");
}

#[test]
fn trim_parent_messages_budget_uses_structured_size() {
    // Build an assistant message where the ToolUse input is large enough that
    // estimate_parts_size exceeds max_chars, but content.len() would not.
    // max_chars = 10 — any real message will exceed it.
    let large_input = json!({"cmd": "x".repeat(200)});
    let assistant_msg = Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: "tu_x".to_owned(),
            name: "shell".to_owned(),
            input: large_input,
        }],
    );
    // Sanity: estimate_parts_size is larger than content.len() for the structured message.
    let estimated = estimate_parts_size(&assistant_msg);
    assert!(
        estimated > assistant_msg.content.len(),
        "structured size ({estimated}) must exceed flat content ({})",
        assistant_msg.content.len()
    );

    let mut msgs = vec![assistant_msg, text_msg(Role::User, "hi")];
    trim_parent_messages(&mut msgs, 10); // tiny budget — triggers truncation
    assert!(
        msgs.len() < 2,
        "budget truncation must fire based on structured size"
    );
}

#[test]
fn trim_parent_messages_removes_empty_message_after_pruning() {
    // A user message with only ToolResult parts that are all orphaned becomes empty
    // and must be removed from the slice entirely.
    let mut msgs = vec![
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "tu_orphan".to_owned(),
                content: "result".to_owned(),
                is_error: false,
            }],
        ),
        text_msg(Role::Assistant, "reply"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert!(
        msgs.iter()
            .all(|m| m.role != Role::User || !m.parts.is_empty()),
        "emptied user messages must be removed"
    );
    let has_orphan = msgs.iter().flat_map(|m| m.parts.iter()).any(
        |p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "tu_orphan"),
    );
    assert!(!has_orphan, "orphaned ToolResult must not survive");
}

#[test]
fn orphan_pruning_preserves_thinking_block() {
    // Assistant message: [ThinkingBlock, Text, ToolUse(matched)]
    // User message:      [ToolResult(matched)]
    // After pruning: ToolUse is matched → nothing removed → rebuild_content NOT called →
    //                ThinkingBlock text in content must be intact.
    let thinking_text = "deep reasoning here";
    let assistant_msg = Message::from_parts(
        Role::Assistant,
        vec![
            MessagePart::ThinkingBlock {
                thinking: thinking_text.to_owned(),
                signature: "sig123".to_owned(),
            },
            MessagePart::Text {
                text: "answer".to_owned(),
            },
            MessagePart::ToolUse {
                id: "tu_matched".to_owned(),
                name: "shell".to_owned(),
                input: json!({}),
            },
        ],
    );
    // Capture content before pruning — it must not change.
    let content_before = assistant_msg.content.clone();

    let mut msgs = vec![
        assistant_msg,
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "tu_matched".to_owned(),
                content: "ok".to_owned(),
                is_error: false,
            }],
        ),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);

    assert_eq!(msgs.len(), 2, "no messages should be removed");
    assert_eq!(msgs[0].parts.len(), 3, "all 3 assistant parts must survive");
    assert_eq!(
        msgs[0].content, content_before,
        "content must not be modified (ThinkingBlock must not be erased)"
    );
}

#[test]
fn trailing_assistant_tool_use_preserved_without_result() {
    // The slice ends with an assistant ToolUse that has no corresponding ToolResult —
    // this is the trailing-edge case where the slice ends before the result arrives.
    // Pass 2 must NOT remove this ToolUse.
    let mut msgs = vec![
        text_msg(Role::User, "do something"),
        tool_use_msg("tu_trailing", "shell"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(msgs.len(), 2, "both messages must be preserved");
    let has_trailing_use = msgs[1]
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "tu_trailing"));
    assert!(
        has_trailing_use,
        "trailing unanswered ToolUse must not be pruned"
    );
}

#[test]
fn budget_keeps_suffix_not_prefix() {
    // With a tight budget that fits only 1 message, the LAST (most recent) message must
    // be kept, not the first (oldest).  This verifies the suffix-first truncation direction.
    let small = text_msg(Role::User, "recent"); // ~6 bytes
    let large = text_msg(Role::User, "x".repeat(500).as_str()); // ~500 bytes
    let small_size = estimate_parts_size(&small);
    let large_size = estimate_parts_size(&large);
    let budget = small_size + large_size / 2; // fits small but not large
    let mut msgs = vec![large, small]; // large first (older), small second (newer)
    trim_parent_messages(&mut msgs, budget);
    assert_eq!(msgs.len(), 1, "only one message must fit");
    assert_eq!(
        msgs[0].content, "recent",
        "the most recent (suffix) message must be kept, not the older one"
    );
}

#[test]
fn trim_parent_messages_partial_prune_keeps_text() {
    // A user message with both an orphaned ToolResult AND a Text part.
    // After pruning, the ToolResult is removed but the Text part — and the message — must survive.
    let mut msgs = vec![
        text_msg(Role::Assistant, "thinking..."),
        Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "tu_gone".to_owned(),
                    content: "old result".to_owned(),
                    is_error: false,
                },
                MessagePart::Text {
                    text: "also some user text".to_owned(),
                },
            ],
        ),
        text_msg(Role::Assistant, "ok"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    // Message must still exist (not emptied).
    let user_msg = msgs.iter().find(|m| m.role == Role::User);
    assert!(
        user_msg.is_some(),
        "user message must survive partial pruning"
    );
    let user_msg = user_msg.unwrap();
    // The ToolResult must be gone.
    let has_orphan = user_msg
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { .. }));
    assert!(!has_orphan, "orphaned ToolResult must be removed");
    // The Text part must remain.
    let has_text = user_msg
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::Text { text } if text == "also some user text"));
    assert!(has_text, "Text part must survive after orphan removal");
}
