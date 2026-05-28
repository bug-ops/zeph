// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{Message, Role};

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
