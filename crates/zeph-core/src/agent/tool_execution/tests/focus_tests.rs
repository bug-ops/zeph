// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role, ToolUseRequest};

use crate::agent::Agent;
use crate::agent::tests::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn make_agent() -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent
}

/// Regression test for #4558: `select_messages_for_compression` must return `to_compress`
/// in ascending chronological index order regardless of `HashSet` iteration order.
///
/// Before the fix, indices were collected from a raw `HashSet` into `to_compress` without
/// sorting, producing non-deterministic message ordering in the compression prompt.
#[test]
fn select_messages_for_compression_returns_chronological_order() {
    let mut agent = make_agent();

    // Build a history: system (idx 0) + 10 user/assistant messages (idx 1..=10).
    // system is at idx 0 and is filtered out by the role guard, so all 10 are compressible.
    for i in 1..=10u32 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        agent
            .msg
            .messages
            .push(Message::from_legacy(role, format!("message {i}")));
    }

    // preserve_tail = 2 means the last 2 compressible messages are kept; the rest (8) are compressed.
    let preserve_tail = 2;
    let result = agent.select_messages_for_compression(preserve_tail);

    let (_, to_compress) = result.expect("enough messages to compress");

    // The content of each message encodes its original position ("message N").
    // Extract the numeric suffix and verify strict ascending order.
    let positions: Vec<u32> = to_compress
        .iter()
        .map(|m| {
            m.content
                .strip_prefix("message ")
                .and_then(|s| s.parse::<u32>().ok())
                .expect("message content must be 'message N'")
        })
        .collect();

    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "to_compress must be in ascending chronological order (regression for #4558); \
         got: {positions:?}"
    );
}

/// Verify that `select_messages_for_compression` excludes focus-pinned messages.
#[test]
fn select_messages_for_compression_excludes_pinned() {
    let mut agent = make_agent();

    for i in 1..=8u32 {
        let mut msg = Message::from_legacy(Role::User, format!("msg {i}"));
        if i == 3 || i == 5 {
            msg.metadata.focus_pinned = true;
        }
        agent.msg.messages.push(msg);
    }

    let result = agent.select_messages_for_compression(1);
    let (to_remove, to_compress) = result.expect("enough messages to compress");

    // Pinned messages must not appear in the removal set or the compression slice.
    let msg_at = |idx: usize| agent.msg.messages[idx].content.clone();
    for idx in &to_remove {
        let content = msg_at(*idx);
        assert!(
            !agent.msg.messages[*idx].metadata.focus_pinned,
            "pinned message '{content}' must not be in to_remove"
        );
    }
    for m in &to_compress {
        assert!(
            !m.metadata.focus_pinned,
            "pinned message '{}' must not appear in to_compress",
            m.content
        );
    }
}

fn tool_use_request(id: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.to_owned(),
        name: "bash".to_owned().into(),
        input: serde_json::json!({}),
    }
}

fn push_tool_result(agent: &mut Agent<MockChannel>, id: &str, content: &str, is_error: bool) {
    let part = MessagePart::ToolResult {
        tool_use_id: id.to_owned(),
        content: content.to_owned(),
        is_error,
    };
    agent.msg.messages.push(Message {
        role: Role::User,
        content: format!("[tool_result: {id}]\n{content}"),
        parts: vec![part],
        metadata: MessageMetadata::default(),
    });
}

fn tool_result_count(agent: &Agent<MockChannel>, id: &str) -> usize {
    agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == id))
        .count()
}

/// Regression test for #5513 item 5: `persist_cancelled_tool_results` must be a complete
/// no-op — no new message pushed at all — when every `tool_use_id` in the batch already has
/// a `ToolResult` (real or tombstone) in history. Before the idempotency guard was added, a
/// caller invoking this a second time for the same batch (as the pre-fix `tier_loop.rs`
/// cascading-cancellation bug did) would append a duplicate/contradicting `ToolResult`.
#[tokio::test]
async fn persist_cancelled_tool_results_is_noop_when_all_ids_already_resolved() {
    let mut agent = make_agent();
    push_tool_result(&mut agent, "call-1", "real output", false);
    let message_count_before = agent.msg.messages.len();

    agent
        .persist_cancelled_tool_results(&[tool_use_request("call-1")], None)
        .await;

    assert_eq!(
        agent.msg.messages.len(),
        message_count_before,
        "no new message must be pushed when the id is already resolved"
    );
    assert_eq!(
        tool_result_count(&agent, "call-1"),
        1,
        "the original real ToolResult must not be duplicated"
    );
}

/// Regression test for #5513 item 5: when a batch mixes already-resolved and unresolved
/// `tool_use_id`s, `persist_cancelled_tool_results` must tombstone only the unresolved ones.
#[tokio::test]
async fn persist_cancelled_tool_results_only_tombstones_unresolved_ids() {
    let mut agent = make_agent();
    push_tool_result(&mut agent, "call-1", "real output", false);

    agent
        .persist_cancelled_tool_results(
            &[tool_use_request("call-1"), tool_use_request("call-2")],
            None,
        )
        .await;

    assert_eq!(
        tool_result_count(&agent, "call-1"),
        1,
        "already-resolved call-1 must not receive a second ToolResult"
    );
    assert_eq!(
        tool_result_count(&agent, "call-2"),
        1,
        "unresolved call-2 must receive exactly one tombstone ToolResult"
    );
    let call2_is_tombstone = agent.msg.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::ToolResult { tool_use_id, content, is_error }
                    if tool_use_id == "call-2" && content == "[Cancelled]" && *is_error
            )
        })
    });
    assert!(
        call2_is_tombstone,
        "call-2's ToolResult must be the [Cancelled] tombstone"
    );
}

/// Regression test for #5513 item 5: with no pre-existing results at all, every id in the
/// batch must still receive a tombstone (the guard must not become a universal no-op).
#[tokio::test]
async fn persist_cancelled_tool_results_tombstones_all_ids_when_none_resolved() {
    let mut agent = make_agent();

    agent
        .persist_cancelled_tool_results(
            &[tool_use_request("call-1"), tool_use_request("call-2")],
            None,
        )
        .await;

    assert_eq!(tool_result_count(&agent, "call-1"), 1);
    assert_eq!(tool_result_count(&agent, "call-2"), 1);
}

/// Regression test for the Ollama id-reuse finding (impl-critic, verified against
/// `crates/zeph-llm/src/ollama.rs:462`): Ollama assigns `tool_call` ids as `format!("call_{i}")`
/// by batch index, so `call_0` legitimately recurs on *every* turn of a multi-turn tool
/// conversation — unlike OpenAI/Claude/Gemini, which use globally unique per-call ids.
///
/// The item-5 idempotency guard (S1 fix) scopes its "already resolved" scan to messages from
/// the current turn's assistant `ToolUse` message onward (the most recent `Role::Assistant`
/// message), not the whole history — mirroring production, where
/// `push_assistant_tool_use_message` always pushes that message before any cancellation path
/// can call `persist_cancelled_tool_results`. So turn 1's resolved `call_0` no longer shadows
/// turn 2's unrelated dispatch of the same id.
#[tokio::test]
async fn persist_cancelled_tool_results_writes_tombstone_for_id_reused_in_a_new_turn() {
    let mut agent = make_agent();
    // Turn 1: call_0 already resolved with a real result (Ollama-style id, batch index 0).
    push_tool_result(&mut agent, "call_0", "turn-1 output", false);

    // Turn 2: push_assistant_tool_use_message's effect — the current turn's assistant ToolUse
    // message reusing the same id "call_0" (Ollama assigns ids by batch index, not globally).
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use: bash(call_0)]".to_owned(),
        parts: vec![MessagePart::ToolUse {
            id: "call_0".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });

    // Turn 2's dispatch gets cancelled before completion.
    agent
        .persist_cancelled_tool_results(&[tool_use_request("call_0")], None)
        .await;

    assert_eq!(
        tool_result_count(&agent, "call_0"),
        2,
        "turn 2's cancelled call_0 must still receive its own tombstone ToolResult, \
         separate from turn 1's real result — the guard must not treat a new turn's \
         id-reused call as already resolved"
    );
}

/// #5646 regression: `persist_cancelled_tool_results`'s `insert_at: Some(index)` branch must
/// splice the tombstone message at that exact index rather than appending it at the true end —
/// direct coverage of the branch itself, complementing the indirect coverage via
/// `flush_orphaned_tests::flush_orphaned_inserts_tombstone_immediately_after_orphan_not_at_end`,
/// which only exercises it through `flush_orphaned_tool_use_on_shutdown`.
#[tokio::test]
async fn persist_cancelled_tool_results_some_index_inserts_at_that_position() {
    let mut agent = make_agent();

    // system (idx 0), assistant ToolUse (idx 1), a later unrelated message already appended
    // after it (idx 2) — mirrors the #5646 shape where a later turn's message lands after the
    // still-orphaned assistant ToolUse before the tombstone is spliced in.
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".to_owned(),
        parts: vec![MessagePart::ToolUse {
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    let orphan_idx = agent.msg.messages.len() - 1;
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "later unrelated message".to_owned(),
        parts: vec![MessagePart::Text {
            text: "later unrelated message".to_owned(),
        }],
        metadata: MessageMetadata::default(),
    });
    let messages_before = agent.msg.messages.len();

    agent
        .persist_cancelled_tool_results(&[tool_use_request("call-1")], Some(orphan_idx + 1))
        .await;

    assert_eq!(
        agent.msg.messages.len(),
        messages_before + 1,
        "exactly one tombstone message must be inserted"
    );
    assert!(
        agent.msg.messages[orphan_idx + 1]
            .parts
            .iter()
            .any(|p| matches!(
                p,
                MessagePart::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == "call-1" && *is_error
            )),
        "the tombstone must be spliced in at insert_at, not appended at the true end"
    );
    assert_eq!(
        agent.msg.messages[orphan_idx + 2].content,
        "later unrelated message",
        "the later message must be pushed one slot forward by the insertion, not displaced"
    );
}
