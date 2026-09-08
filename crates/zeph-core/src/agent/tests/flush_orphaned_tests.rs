// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

async fn flush_test_memory() -> SemanticMemory {
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
    SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        provider,
        "test-model",
    )
    .await
    .unwrap()
}

/// FO1: no-op when the message list has no assistant message.
#[tokio::test]
async fn flush_orphaned_noop_when_no_assistant_message() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    // Push only a user message — no assistant message.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![MessagePart::Text { text: "hi".into() }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();
    assert!(
        history.is_empty(),
        "no tombstone must be persisted when there is no assistant message"
    );
}

/// FO2: no-op when the last assistant message contains no `ToolUse` parts.
#[tokio::test]
async fn flush_orphaned_noop_when_no_tool_use_parts() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "just text".into(),
        parts: vec![MessagePart::Text {
            text: "just text".into(),
        }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();
    assert!(
        history.is_empty(),
        "no tombstone must be persisted when there are no ToolUse parts"
    );
}

/// FO3: tombstone `ToolResult` is persisted for each unpaired `ToolUse`.
#[tokio::test]
async fn flush_orphaned_persists_tombstone_for_unpaired_tool_use() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![
            MessagePart::ToolUse {
                id: "orphan_1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            },
            MessagePart::ToolUse {
                id: "orphan_2".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            },
        ],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();

    assert_eq!(
        history.len(),
        1,
        "exactly one tombstone user message must be persisted"
    );
    assert_eq!(history[0].role, Role::User);
    for id in ["orphan_1", "orphan_2"] {
        assert!(
            history[0].parts.iter().any(|p| matches!(
                p,
                MessagePart::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == id && *is_error
            )),
            "tombstone ToolResult for {id} must be is_error=true"
        );
    }
}

/// FO4: no-op when all `ToolUse` ids are already covered by a following `ToolResult`.
#[tokio::test]
async fn flush_orphaned_noop_when_tool_use_already_paired() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![MessagePart::ToolUse {
            id: "paired_id".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "[tool_result]".into(),
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "paired_id".into(),
            content: "ok".into(),
            is_error: false,
        }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();
    assert!(
        history.is_empty(),
        "no tombstone must be persisted when all ToolUse parts are already paired"
    );
}

/// FO5 (#5646 regression): if a later turn's message has already been appended after the
/// still-orphaned assistant `ToolUse` by the time shutdown runs (e.g. resume-then-dispatch
/// before the previous orphan was sanitized away), the tombstone must be spliced in
/// immediately after the orphan — not appended at the true end of history, which would leave
/// the `ToolUse` still not immediately followed by its `ToolResult`.
#[tokio::test]
async fn flush_orphaned_inserts_tombstone_immediately_after_orphan_not_at_end() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    let messages_before = agent.msg.messages.len();
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![MessagePart::ToolUse {
            id: "orphan_1".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    let orphan_idx = agent.msg.messages.len() - 1;
    // Simulates a later, unrelated turn's message already appended after the orphan before
    // shutdown fires (the exact #5646 shape).
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "a later, unrelated turn's message".into(),
        parts: vec![MessagePart::Text {
            text: "a later, unrelated turn's message".into(),
        }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    assert_eq!(
        agent.msg.messages.len(),
        messages_before + 3,
        "the tombstone must be added without displacing the later message"
    );
    assert!(
        agent.msg.messages[orphan_idx + 1].parts.iter().any(
            |p| matches!(p, MessagePart::ToolResult { tool_use_id, is_error, .. }
                if tool_use_id == "orphan_1" && *is_error)
        ),
        "the tombstone must be spliced in immediately after the orphan, not appended after the \
         later message"
    );
    assert_eq!(
        agent.msg.messages[orphan_idx + 2].content,
        "a later, unrelated turn's message",
        "the later message must remain after the tombstone, not before it"
    );
}

/// FO6 (#6783 fix): `flush_orphaned_tool_use_on_shutdown` computes `unpaired_ids` via
/// `zeph_llm::tool_pairing::unmatched_tool_use_ids` (adjacency-scoped, immediate-neighbour only)
/// and correctly identifies `call_0` as unpaired in this shape. `Agent::persist_cancelled_tool_results`
/// (`crates/zeph-core/src/agent/tool_execution/focus.rs`) now uses the same adjacency-scoped
/// `zeph_llm::tool_pairing::resolved_tool_result_ids` for its own idempotency guard instead of
/// scanning `self.msg.messages[turn_start..]` to the true end of history, so a later, unrelated
/// `ToolResult` reusing `call_0` no longer falls inside the scan and can no longer be treated as
/// "already resolved" to silently swallow the tombstone.
#[tokio::test]
async fn flush_orphaned_writes_tombstone_despite_a_later_unrelated_reuse_of_the_same_id() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    // Genuinely in-flight ToolUse — the last assistant message.
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![MessagePart::ToolUse {
            id: "call_0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    // Immediately following: unrelated plain text, not the reply to call_0.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "an unrelated later message".into(),
        parts: vec![MessagePart::Text {
            text: "an unrelated later message".into(),
        }],
        metadata: MessageMetadata::default(),
    });
    // A ToolResult reusing "call_0" surfaces even later (Ollama-style id reuse from an
    // unrelated call). Adjacency correctly ignores this — it is not the immediate neighbour.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "[tool_result]".into(),
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "call_0".into(),
            content: "unrelated reused result".into(),
            is_error: false,
        }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();

    let tombstone_written = history.iter().any(|m| {
        m.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::ToolResult { tool_use_id, is_error, content }
                    if tool_use_id == "call_0" && *is_error && content == "[Cancelled]"
            )
        })
    });
    assert!(
        tombstone_written,
        "the genuinely in-flight call_0 must get a tombstone, not be silently treated as \
         paired against an unrelated later reuse of its id"
    );
}

/// FO7: adjacency must not be confused by an *earlier* turn reusing the same id. A call that is
/// directly, immediately paired with its own `ToolResult` must produce no tombstone, even when an
/// unrelated earlier turn already used and resolved the same `call_0` id (Ollama-style reuse).
/// `unmatched_tool_use_ids` only ever looks at the immediate neighbour, so it cannot be swayed by
/// anything earlier in history — this is a direct regression test for that contract.
#[tokio::test]
async fn flush_orphaned_noop_when_directly_paired_despite_an_earlier_turn_reusing_the_same_id() {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    let provider = mock_provider(vec![]);
    let memory = flush_test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        100,
    );

    // Turn 1: call_0 used and legitimately resolved.
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![MessagePart::ToolUse {
            id: "call_0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "[tool_result]".into(),
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "call_0".into(),
            content: "turn-1 output".into(),
            is_error: false,
        }],
        metadata: MessageMetadata::default(),
    });
    // Turn 2: call_0 reused (Ollama batch-index id), immediately and legitimately paired — this
    // is the last assistant message the function will examine.
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "[tool_use]".into(),
        parts: vec![MessagePart::ToolUse {
            id: "call_0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "[tool_result]".into(),
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "call_0".into(),
            content: "turn-2 output".into(),
            is_error: false,
        }],
        metadata: MessageMetadata::default(),
    });

    agent.flush_orphaned_tool_use_on_shutdown().await;

    let history = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();
    assert!(
        history.is_empty(),
        "no tombstone must be persisted for a call that is directly, immediately paired, \
         regardless of an earlier turn reusing the same id"
    );
}
