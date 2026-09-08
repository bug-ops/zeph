// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde_json::json;
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::agent::subagent_commands::{estimate_parts_size, trim_parent_messages};

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
fn trim_parent_messages_keeps_matched_tool_pairs() {
    // Both ToolUse and ToolResult for "tu_B" are in the slice — both must be kept. A leading
    // User anchor keeps the window's first non-system message role=user, so the D3 leading-role
    // cascade (see `leading_matched_pair_survives_as_downgraded_anchor`) does not fire here.
    let mut msgs = vec![
        text_msg(Role::User, "run it"),
        tool_use_msg("tu_B", "shell"),
        tool_result_msg("tu_B", "output-b"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(msgs.len(), 3, "matched pair must not be removed");
    let has_use = msgs[1]
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "tu_B"));
    let has_result = msgs[2]
        .parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "tu_B"));
    assert!(has_use, "ToolUse must be preserved");
    assert!(has_result, "ToolResult must be preserved");
}

#[test]
fn leading_matched_pair_survives_as_downgraded_anchor() {
    // C1 (architect resolution): a leading ToolUse/ToolResult pair with no preceding User
    // anchor would be destroyed entirely by Delete's leading-role cascade (dropping the leading
    // Assistant orphans its ToolResult, which is then deleted too — proven unsatisfiable by
    // deletion alone for this shape). `trim_parent_messages` detects the would-be annihilation
    // on a scratch copy and falls back to `DowngradeToText` on the original instead: the leading
    // Assistant(ToolUse) message is still dropped by the leading-role rule, but its result
    // survives downgraded to a Text anchor rather than the whole window being destroyed.
    let mut msgs = vec![
        tool_use_msg("tu_B", "shell"),
        tool_result_msg("tu_B", "output-b"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(
        msgs.len(),
        1,
        "the leading Assistant is still dropped, but the result survives as a text anchor"
    );
    assert_eq!(msgs[0].role, Role::User);
    let has_marker = msgs[0].parts.iter().any(
        |p| matches!(p, MessagePart::Text { text } if text.contains("tu_B") && text.contains("output-b")),
    );
    assert!(
        has_marker,
        "the downgraded ToolResult content must survive as a marked Text part"
    );
    assert!(
        !msgs[0]
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. })),
        "no dangling native ToolResult may remain"
    );
}

#[test]
fn non_annihilating_window_still_deletes_not_downgrades() {
    // S5 preservation (C1 architect handoff #4): the DowngradeToText fallback is a bounded
    // exception triggered only by would-be annihilation, not a general policy change — a window
    // where Delete does NOT empty everything must still delete the orphan outright, never
    // downgrade it to a text marker.
    let mut msgs = vec![
        text_msg(Role::User, "hi"),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "tu_A".to_owned(),
                content: "result".to_owned(),
                is_error: false,
            }],
        ),
        text_msg(Role::Assistant, "ok"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(
        msgs.len(),
        2,
        "the orphaned message is removed outright, the rest survive"
    );
    for m in &msgs {
        assert!(
            m.parts.iter().all(
                |p| !matches!(p, MessagePart::Text { text } if text.starts_with("[tool result: "))
            ),
            "Delete must not downgrade when the window does not annihilate"
        );
    }
    assert_eq!(msgs[0].content, "hi");
    assert_eq!(msgs[1].content, "ok");
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
fn leading_orphan_only_window_is_downgraded_not_annihilated() {
    // C1: a leading orphaned ToolResult (no preceding ToolUse) followed only by trailing
    // Assistant text is the same annihilation shape as
    // `leading_matched_pair_survives_as_downgraded_anchor` — under plain Delete the orphan is
    // stripped, emptying the message, which then also gets dropped by the leading-role rule
    // once the trailing Assistant text becomes the new leading (non-user) message, annihilating
    // the whole window. The fallback downgrades the orphan to a Text anchor instead, so both
    // messages survive.
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
    assert_eq!(msgs.len(), 2, "the fallback preserves both messages");
    assert_eq!(msgs[0].role, Role::User);
    let has_marker = msgs[0].parts.iter().any(
        |p| matches!(p, MessagePart::Text { text } if text.contains("tu_orphan") && text.contains("result")),
    );
    assert!(
        has_marker,
        "the orphan must survive downgraded to a marked Text anchor"
    );
    assert!(
        !msgs[0]
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. })),
        "no dangling native ToolResult may remain"
    );
    assert_eq!(msgs[1].content, "reply");
}

#[test]
fn orphan_pruning_preserves_thinking_block() {
    // Assistant message: [ThinkingBlock, Text, ToolUse(matched)] — preceded by a User anchor so
    // the window's leading message has role=user (the D3 leading-role cascade, see
    // `leading_matched_pair_survives_as_downgraded_anchor`, does not fire here).
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
        text_msg(Role::User, "please investigate"),
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

    assert_eq!(msgs.len(), 3, "no messages should be removed");
    assert_eq!(msgs[1].parts.len(), 3, "all 3 assistant parts must survive");
    assert_eq!(
        msgs[1].content, content_before,
        "content must not be modified (ThinkingBlock must not be erased)"
    );
}

#[test]
fn trailing_assistant_tool_use_is_stripped_as_orphaned() {
    // S1 (v2 design, issue #6771): universal Repair applies regardless of position — a trailing,
    // unanswered ToolUse in the parent-context snapshot handed to a fresh subagent can never be
    // answered (the subagent has no way to supply the parent's tool result), so it is orphaned
    // like any other and stripped. This is a behavior change from the old v1 "trailing exemption".
    let mut msgs = vec![
        text_msg(Role::User, "do something"),
        tool_use_msg("tu_trailing", "shell"),
    ];
    trim_parent_messages(&mut msgs, usize::MAX);
    assert_eq!(
        msgs.len(),
        1,
        "the trailing unanswered ToolUse message must be removed"
    );
    assert_eq!(msgs[0].content, "do something");
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
