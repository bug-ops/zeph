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
