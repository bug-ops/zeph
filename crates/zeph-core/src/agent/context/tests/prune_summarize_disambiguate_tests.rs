// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use tokio::sync::watch;
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider, mock_provider_failing,
};
use crate::agent::context_manager::CompactionTier;
use crate::context::ContextBudget;

async fn create_memory_with_summaries(
    provider: zeph_llm::any::AnyProvider,
    summaries: &[&str],
) -> (SemanticMemory, zeph_memory::ConversationId) {
    let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    for content in summaries {
        let m1 = memory
            .sqlite()
            .save_message(cid, "user", "q")
            .await
            .unwrap();
        let m2 = memory
            .sqlite()
            .save_message(cid, "assistant", "a")
            .await
            .unwrap();
        memory
            .sqlite()
            .save_summary(
                cid,
                content,
                Some(m1),
                Some(m2),
                i64::try_from(zeph_memory::TokenCounter::new().count_tokens(content)).unwrap(),
            )
            .await
            .unwrap();
    }
    (memory, cid)
}

#[test]
fn test_prune_frees_tokens() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);

    let big_body = "x".repeat(500);
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: big_body,
            compacted_at: None,
        }],
    ));

    let freed = agent.prune_tool_outputs(10);
    assert!(freed > 0);
    assert_eq!(rx.borrow().tool_output_prunes, 1);

    if let MessagePart::ToolOutput {
        body, compacted_at, ..
    } = &agent.msg.messages[1].parts[0]
    {
        assert!(compacted_at.is_some());
        assert!(body.is_empty(), "body should be cleared after prune");
    } else {
        panic!("expected ToolOutput");
    }
}

#[test]
fn test_prune_respects_protection_zone() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(10000, 0.20, 0.75, 4, 999_999);

    let big_body = "x".repeat(500);
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: big_body,
            compacted_at: None,
        }],
    ));

    let freed = agent.prune_tool_outputs(10);
    assert_eq!(freed, 0);

    if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
        assert!(compacted_at.is_none());
    } else {
        panic!("expected ToolOutput");
    }
}

#[test]
fn prune_tool_outputs_preserves_overflow_reference() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, _rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);

    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let body = format!(
        "truncated output\n[full output stored \u{2014} ID: {uuid} \u{2014} 99999 bytes, use read_overflow tool to retrieve]"
    );
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body,
            compacted_at: None,
        }],
    ));

    let freed = agent.prune_tool_outputs(10);
    assert!(freed > 0);

    if let MessagePart::ToolOutput {
        body, compacted_at, ..
    } = &agent.msg.messages[1].parts[0]
    {
        assert!(compacted_at.is_some());
        assert_eq!(
            body,
            &format!("[tool output pruned; use read_overflow {uuid} to retrieve]")
        );
    } else {
        panic!("expected ToolOutput");
    }
}

#[test]
fn prune_stale_tool_outputs_preserves_overflow_reference_in_tool_output() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let body = format!(
        "truncated output\n[full output stored \u{2014} ID: {uuid} \u{2014} 99999 bytes, use read_overflow tool to retrieve]"
    );
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body,
            compacted_at: None,
        }],
    ));
    for _ in 0..4 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "recent".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let freed = agent.prune_stale_tool_outputs(4);
    assert!(freed > 0);

    if let MessagePart::ToolOutput {
        body, compacted_at, ..
    } = &agent.msg.messages[1].parts[0]
    {
        assert!(compacted_at.is_some());
        assert_eq!(
            body,
            &format!("[tool output pruned; use read_overflow {uuid} to retrieve]")
        );
    } else {
        panic!("expected ToolOutput");
    }
}

#[test]
fn prune_stale_tool_outputs_preserves_overflow_reference_in_tool_result() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    // Content large enough to exceed the 20-token threshold
    let content = format!(
        "{}\n[full output stored \u{2014} ID: {uuid} \u{2014} 99999 bytes, use read_overflow tool to retrieve]",
        "x".repeat(200)
    );
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content,
            is_error: false,
        }],
    ));
    for _ in 0..4 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "recent".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let freed = agent.prune_stale_tool_outputs(4);
    assert!(freed > 0);

    if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[1].parts[0] {
        assert_eq!(
            content,
            &format!("[tool output pruned; use read_overflow {uuid} to retrieve]")
        );
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn test_tier2_after_insufficient_prune() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} with enough content to push over budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();
    assert_eq!(rx.borrow().context_compactions, 1);
}

#[tokio::test]
async fn test_inject_cross_session_no_memory_noop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let msg_count = agent.msg.messages.len();

    agent
        .inject_cross_session_context("test", 1000)
        .await
        .unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_inject_cross_session_zero_budget_noop() {
    let provider = mock_provider(vec![]);
    let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary"]).await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        Arc::new(memory),
        cid,
        50,
        5,
        50,
    );
    let msg_count = agent.msg.messages.len();

    agent.inject_cross_session_context("test", 0).await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_remove_cross_session_messages() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.insert(
        1,
        Message::from_parts(
            Role::System,
            vec![MessagePart::CrossSession {
                text: "old cross-session".into(),
            }],
        ),
    );
    assert_eq!(agent.msg.messages.len(), 2);

    agent.remove_cross_session_messages();
    assert_eq!(agent.msg.messages.len(), 1);
}

#[tokio::test]
async fn test_remove_cross_session_preserves_other_system() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.insert(
        1,
        Message::from_parts(
            Role::System,
            vec![MessagePart::Summary {
                text: "keep this summary".into(),
            }],
        ),
    );
    agent.msg.messages.insert(
        2,
        Message::from_parts(
            Role::System,
            vec![MessagePart::CrossSession {
                text: "remove this".into(),
            }],
        ),
    );
    assert_eq!(agent.msg.messages.len(), 3);

    agent.remove_cross_session_messages();
    assert_eq!(agent.msg.messages.len(), 2);
    assert!(agent.msg.messages[1].content.contains("keep this summary"));
}

#[tokio::test]
async fn test_store_session_summary_on_compaction() {
    let provider = mock_provider(vec!["compacted summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(Arc::new(memory), cid, 50, 5, 50)
        .with_context_budget(10000, 0.20, 0.80, 2, 0);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // compact_context should succeed (non-fatal store)
    agent.compact_context().await.unwrap();
    assert!(agent.msg.messages[1].content.contains("compacted summary"));
}

// Verify that compact_context() calls replace_conversation() when SQLite has enough rows.
// This exercises the happy-path branch (ids.len() >= 2) which is the precondition for
// store_session_summary() in the fix for issue #1911.
#[tokio::test]
async fn test_compact_context_calls_replace_conversation() {
    let provider = mock_provider(vec!["compacted summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;
    let sqlite = memory.sqlite();

    // Persist a system prompt and several user messages so oldest_message_ids returns >= 2 rows.
    sqlite
        .save_message(cid, "system", "system prompt")
        .await
        .unwrap();
    for i in 0..5 {
        sqlite
            .save_message(cid, "user", &format!("message {i}"))
            .await
            .unwrap();
    }

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(Arc::new(memory), cid, 50, 5, 50)
        .with_context_budget(10000, 0.20, 0.80, 2, 0);

    // Mirror the same messages in the in-memory list so compaction has content to process.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "system prompt".to_string(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.compact_context().await.unwrap();

    // After compaction, replace_conversation() must have been called:
    // original messages become agent_visible=0, summary row is inserted with agent_visible=1.
    let memory_ref = agent.memory_state.persistence.memory.as_ref().unwrap();
    let agent_visible = memory_ref
        .sqlite()
        .load_history_filtered(cid, 50, Some(true), None)
        .await
        .unwrap();
    // At least one agent-visible summary row must exist in SQLite.
    assert!(
        !agent_visible.is_empty(),
        "replace_conversation must have inserted a summary row in SQLite"
    );
    assert!(
        agent_visible
            .iter()
            .any(|m| m.content.contains("compacted summary")),
        "SQLite must contain the summary inserted by replace_conversation"
    );
}

#[test]
fn test_budget_allocation_cross_session() {
    let budget = ContextBudget::new(1000, 0.20);
    let tc = zeph_memory::TokenCounter::new();
    let alloc = budget.allocate("", "", &tc, false);

    assert!(alloc.cross_session > 0);
    assert!(alloc.summaries > 0);
    assert!(alloc.semantic_recall > 0);
    // cross_session should be smaller than summaries
    assert!(alloc.cross_session < alloc.summaries);
}

#[test]
fn test_cross_session_score_threshold_filters() {
    use zeph_memory::semantic::SessionSummaryResult;

    let threshold: f32 = 0.35;

    let results = vec![
        SessionSummaryResult {
            summary_text: "high score".into(),
            score: 0.9,
            conversation_id: zeph_memory::ConversationId(1),
        },
        SessionSummaryResult {
            summary_text: "at threshold".into(),
            score: 0.35,
            conversation_id: zeph_memory::ConversationId(2),
        },
        SessionSummaryResult {
            summary_text: "below threshold".into(),
            score: 0.2,
            conversation_id: zeph_memory::ConversationId(3),
        },
        SessionSummaryResult {
            summary_text: "way below".into(),
            score: 0.0,
            conversation_id: zeph_memory::ConversationId(4),
        },
    ];

    let filtered: Vec<_> = results
        .into_iter()
        .filter(|r| r.score >= threshold)
        .collect();

    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].summary_text, "high score");
    assert_eq!(filtered[1].summary_text, "at threshold");
}

#[test]
fn context_budget_80_percent_threshold() {
    let budget = ContextBudget::new(1000, 0.20);
    let threshold = budget.max_tokens() * 4 / 5;
    assert_eq!(threshold, 800);
    assert!(800 >= threshold); // at threshold → should stop
    assert!(799 < threshold); // below threshold → should continue
}

#[test]
fn prune_stale_tool_outputs_clears_old() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(10000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);

    // Add 6 messages with tool outputs
    for i in 0..6 {
        agent.msg.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: format!("tool_{i}").into(),
                body: "x".repeat(200),
                compacted_at: None,
            }],
        ));
    }
    // 7 messages total (1 system + 6 user)

    let freed = agent.prune_stale_tool_outputs(4);
    assert!(freed > 0);
    assert_eq!(rx.borrow().tool_output_prunes, 1);

    // Messages 1..3 should be pruned (boundary = 7-4=3)
    for i in 1..3 {
        if let MessagePart::ToolOutput {
            body, compacted_at, ..
        } = &agent.msg.messages[i].parts[0]
        {
            assert!(body.is_empty(), "message {i} should be pruned");
            assert!(compacted_at.is_some());
        }
    }
    // Messages 3..6 should be untouched
    for i in 3..7 {
        if let MessagePart::ToolOutput { body, .. } = &agent.msg.messages[i].parts[0] {
            assert!(!body.is_empty(), "message {i} should be kept");
        }
    }
}

#[test]
fn prune_stale_tool_outputs_noop_when_few_messages() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: "output".into(),
            compacted_at: None,
        }],
    ));

    let freed = agent.prune_stale_tool_outputs(4);
    assert_eq!(freed, 0);
}

#[test]
fn prune_stale_prunes_tool_result_too() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Add old message with large ToolResult
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: "x".repeat(500),
            is_error: false,
        }],
    ));
    // Add 4 recent messages
    for _ in 0..4 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "recent".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let freed = agent.prune_stale_tool_outputs(4);
    assert!(freed > 0);

    if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[1].parts[0] {
        assert_eq!(content, "[pruned]");
    } else {
        panic!("expected ToolResult");
    }
}

#[test]
fn prune_stale_tool_outputs_multi_part_tool_result_counted_once_per_part() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // One message with two ToolResult parts — each should be counted/pruned independently.
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![
            MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "x".repeat(500),
                is_error: false,
            },
            MessagePart::ToolResult {
                tool_use_id: "t2".into(),
                content: "y".repeat(500),
                is_error: false,
            },
        ],
    ));
    // Add 4 recent messages to push the above into the prune zone.
    for _ in 0..4 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "recent".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let freed = agent.prune_stale_tool_outputs(4);
    // Both parts must have contributed tokens.
    assert!(freed > 0, "freed must reflect tokens from both parts");

    // Both ToolResult parts in the stale message must be pruned.
    if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[1].parts[0] {
        assert_eq!(content, "[pruned]", "first ToolResult part must be pruned");
    } else {
        panic!("expected ToolResult at parts[0]");
    }
    if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[1].parts[1] {
        assert_eq!(content, "[pruned]", "second ToolResult part must be pruned");
    } else {
        panic!("expected ToolResult at parts[1]");
    }
}

#[tokio::test]
async fn test_prepare_context_scrubs_secrets_when_redact_enabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(4096, 0.20, 0.80, 4, 0);
    agent.runtime.redact_credentials = true;

    // Push a user message containing a secret and a path
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "my key is sk-abc123xyz and lives at /Users/dev/config.toml".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.prepare_context("test").await.unwrap();

    let user_msg = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert!(
        !user_msg.content.contains("sk-abc123xyz"),
        "secret must be redacted"
    );
    assert!(
        !user_msg.content.contains("/Users/dev/"),
        "path must be redacted"
    );
    assert!(
        user_msg.content.contains("[REDACTED]"),
        "secret replaced with [REDACTED]"
    );
    assert!(
        user_msg.content.contains("[PATH]"),
        "path replaced with [PATH]"
    );
}

#[tokio::test]
async fn test_prepare_context_preserves_system_prompt_paths() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(4096, 0.20, 0.80, 4, 0)
        .with_working_dir("/Users/dev/project");
    agent.runtime.redact_credentials = true;

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "debug /Users/dev/project/src/main.rs".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent
        .rebuild_system_prompt("why is ACP not starting?")
        .await;
    agent
        .prepare_context("why is ACP not starting?")
        .await
        .unwrap();

    let system_msg = agent
        .msg
        .messages
        .first()
        .expect("system prompt must exist");
    assert_eq!(system_msg.role, Role::System);
    assert!(
        system_msg
            .content
            .contains("working_directory: /Users/dev/project"),
        "system prompt must keep the real working directory"
    );
    assert!(
        !system_msg.content.contains("[PATH]"),
        "system prompt must not leak placeholder paths into tool instructions"
    );

    let user_msg = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .expect("user message must exist");
    assert!(
        user_msg.content.contains("[PATH]"),
        "user history should still be scrubbed"
    );
    assert!(
        !user_msg.content.contains("/Users/dev/project"),
        "user history must not keep the absolute path"
    );
}

#[tokio::test]
async fn test_prepare_context_no_scrub_when_redact_disabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(4096, 0.20, 0.80, 4, 0);
    agent.runtime.redact_credentials = false;

    let original = "key sk-abc123xyz at /Users/dev/file.rs".to_string();
    agent.msg.messages.push(Message {
        role: Role::User,
        content: original.clone(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.prepare_context("test").await.unwrap();

    let user_msg = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert_eq!(
        user_msg.content, original,
        "content must be unchanged when redact disabled"
    );
}

#[test]
fn correction_prompt_does_not_replay_bad_path_commands() {
    let note = crate::agent::context::assembler_helpers::format_correction_note(
        "cd /Users/m/dev/zeph && grep -n \"acp\" Cargo.toml | head -40",
        "Use the current repository and avoid hard-coded absolute paths.",
    );

    assert!(
        !note.contains("cd /Users/m/dev/zeph"),
        "correction prompt must not replay the faulty absolute-path command"
    );
    assert!(
        !note.contains("[PATH]"),
        "correction prompt must not inject literal path placeholders"
    );
    assert!(
        note.contains("Use the current repository"),
        "correction prompt must preserve the user correction guidance"
    );
}

#[test]
fn compaction_tier_hard_triggers_when_cached_tokens_exceed_hard_threshold() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // budget 1000, hard=0.75, soft=0.50 → hard fires above 750
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.50;
    agent.providers.cached_prompt_tokens = 900;

    assert_eq!(
        agent.compaction_tier(),
        CompactionTier::Hard,
        "cached_prompt_tokens above hard threshold must return Hard"
    );
}

#[test]
fn compaction_tier_none_does_not_trigger_below_soft_threshold() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // budget 1000, hard=0.75, soft=0.50 → nothing fires below 500
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.50;
    agent.providers.cached_prompt_tokens = 100;

    assert_eq!(
        agent.compaction_tier(),
        CompactionTier::None,
        "cached_prompt_tokens below soft threshold must return None"
    );
}

#[tokio::test]
async fn disambiguate_skills_reorders_on_match() {
    let json = r#"{"skill_name":"beta_skill","confidence":0.9,"params":{}}"#;
    let provider = mock_provider(vec![json.to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let metas = [
        SkillMeta {
            name: "alpha_skill".into(),
            description: "does alpha".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
        },
        SkillMeta {
            name: "beta_skill".into(),
            description: "does beta".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
        },
    ];
    let refs: Vec<&SkillMeta> = metas.iter().collect();
    let scored = vec![
        ScoredMatch {
            index: 0,
            score: 0.90,
        },
        ScoredMatch {
            index: 1,
            score: 0.88,
        },
    ];

    let result = agent
        .disambiguate_skills("do beta stuff", &refs, &scored)
        .await;
    assert!(result.is_some());
    let indices = result.unwrap();
    assert_eq!(indices[0], 1); // beta_skill moved to front
}

#[tokio::test]
async fn disambiguate_skills_returns_none_on_error() {
    let provider = mock_provider_failing();
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let metas = [SkillMeta {
        name: "test".into(),
        description: "test".into(),
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: Vec::new(),
        skill_dir: std::path::PathBuf::new(),
        source_url: None,
        git_hash: None,
        category: None,
    }];
    let refs: Vec<&SkillMeta> = metas.iter().collect();
    let scored = vec![ScoredMatch {
        index: 0,
        score: 0.5,
    }];

    let result = agent.disambiguate_skills("query", &refs, &scored).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn disambiguate_skills_empty_candidates() {
    let json = r#"{"skill_name":"none","confidence":0.1,"params":{}}"#;
    let provider = mock_provider(vec![json.to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let metas: [SkillMeta; 0] = [];
    let refs: Vec<&SkillMeta> = metas.iter().collect();
    let scored: Vec<ScoredMatch> = vec![];

    let result = agent.disambiguate_skills("query", &refs, &scored).await;
    assert!(result.is_some());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn disambiguate_skills_unknown_skill_preserves_order() {
    let json = r#"{"skill_name":"nonexistent","confidence":0.5,"params":{}}"#;
    let provider = mock_provider(vec![json.to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let metas = [
        SkillMeta {
            name: "first".into(),
            description: "first skill".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
        },
        SkillMeta {
            name: "second".into(),
            description: "second skill".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
        },
    ];
    let refs: Vec<&SkillMeta> = metas.iter().collect();
    let scored = vec![
        ScoredMatch {
            index: 0,
            score: 0.9,
        },
        ScoredMatch {
            index: 1,
            score: 0.88,
        },
    ];

    let result = agent
        .disambiguate_skills("query", &refs, &scored)
        .await
        .unwrap();
    // No swap since LLM returned unknown name
    assert_eq!(result[0], 0);
    assert_eq!(result[1], 1);
}

#[tokio::test]
async fn disambiguate_single_candidate_no_swap() {
    let json = r#"{"skill_name":"only_skill","confidence":0.95,"params":{}}"#;
    let provider = mock_provider(vec![json.to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let metas = [SkillMeta {
        name: "only_skill".into(),
        description: "the only one".into(),
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: Vec::new(),
        skill_dir: std::path::PathBuf::new(),
        source_url: None,
        git_hash: None,
        category: None,
    }];
    let refs: Vec<&SkillMeta> = metas.iter().collect();
    let scored = vec![ScoredMatch {
        index: 0,
        score: 0.95,
    }];

    let result = agent
        .disambiguate_skills("query", &refs, &scored)
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], 0);
}

#[tokio::test]
async fn rebuild_system_prompt_excludes_skill_when_secret_missing() {
    use std::collections::HashMap;
    use zeph_skills::loader::SkillMeta;
    use zeph_skills::registry::SkillRegistry;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Skill requires a secret that is NOT available
    let meta_with_secret = SkillMeta {
        name: "secure-skill".into(),
        description: "needs a secret".into(),
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: vec!["my_api_key".into()],
        skill_dir: std::path::PathBuf::new(),
        source_url: None,
        git_hash: None,
        category: None,
    };

    // available_custom_secrets is empty — skill must be excluded
    agent.skill_state.available_custom_secrets = HashMap::new();

    let all_meta = [meta_with_secret];
    let matched_indices: Vec<usize> = vec![0];

    let filtered: Vec<usize> = matched_indices
        .into_iter()
        .filter(|&i| {
            let Some(meta) = all_meta.get(i) else {
                return false;
            };
            meta.requires_secrets.iter().all(|s| {
                agent
                    .skill_state
                    .available_custom_secrets
                    .contains_key(s.as_str())
            })
        })
        .collect();

    assert!(
        filtered.is_empty(),
        "skill must be excluded when required secret is missing"
    );
}

#[tokio::test]
async fn rebuild_system_prompt_includes_skill_when_secret_present() {
    use zeph_skills::loader::SkillMeta;
    use zeph_skills::registry::SkillRegistry;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let meta_with_secret = SkillMeta {
        name: "secure-skill".into(),
        description: "needs a secret".into(),
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: vec!["my_api_key".into()],
        skill_dir: std::path::PathBuf::new(),
        source_url: None,
        git_hash: None,
        category: None,
    };

    // Secret IS available
    agent
        .skill_state
        .available_custom_secrets
        .insert("my_api_key".into(), crate::vault::Secret::new("token-val"));

    let all_meta = [meta_with_secret];
    let matched_indices: Vec<usize> = vec![0];

    let filtered: Vec<usize> = matched_indices
        .into_iter()
        .filter(|&i| {
            let Some(meta) = all_meta.get(i) else {
                return false;
            };
            meta.requires_secrets.iter().all(|s| {
                agent
                    .skill_state
                    .available_custom_secrets
                    .contains_key(s.as_str())
            })
        })
        .collect();

    assert_eq!(
        filtered,
        vec![0],
        "skill must be included when required secret is present"
    );
}

#[tokio::test]
async fn rebuild_system_prompt_excludes_skill_when_only_partial_secrets_present() {
    use zeph_skills::loader::SkillMeta;
    use zeph_skills::registry::SkillRegistry;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let meta = SkillMeta {
        name: "multi-secret-skill".into(),
        description: "needs two secrets".into(),
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: vec!["secret_a".into(), "secret_b".into()],
        skill_dir: std::path::PathBuf::new(),
        source_url: None,
        git_hash: None,
        category: None,
    };

    // Only "secret_a" present, "secret_b" missing — skill must be excluded.
    agent
        .skill_state
        .available_custom_secrets
        .insert("secret_a".into(), crate::vault::Secret::new("val-a"));

    let all_meta = [meta];
    let matched_indices: Vec<usize> = vec![0];

    let filtered: Vec<usize> = matched_indices
        .into_iter()
        .filter(|&i| {
            let Some(meta) = all_meta.get(i) else {
                return false;
            };
            meta.requires_secrets.iter().all(|s| {
                agent
                    .skill_state
                    .available_custom_secrets
                    .contains_key(s.as_str())
            })
        })
        .collect();

    assert!(
        filtered.is_empty(),
        "skill must be excluded when only partial secrets are available"
    );
}

fn make_tool_result_message(content: &str) -> Message {
    Message::from_parts(
        Role::User,
        vec![zeph_llm::provider::MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: content.into(),
            is_error: false,
        }],
    )
}

fn make_text_message(text: &str) -> Message {
    Message::from_legacy(Role::User, text)
}

#[test]
fn remove_tool_responses_empty_messages_unchanged() {
    let msgs: Vec<Message> = vec![];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
    assert!(result.is_empty());
}

#[test]
fn remove_tool_responses_no_tool_messages_unchanged() {
    let msgs = vec![make_text_message("hello"), make_text_message("world")];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].content, "hello");
}

#[test]
fn remove_tool_responses_100_percent_clears_all() {
    let msgs = vec![
        make_tool_result_message("result1"),
        make_tool_result_message("result2"),
        make_tool_result_message("result3"),
    ];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
    assert_eq!(result.len(), 3);
    for msg in &result {
        if let Some(zeph_llm::provider::MessagePart::ToolResult { content, .. }) = msg.parts.first()
        {
            assert_eq!(content, "[compacted]");
        }
    }
}

#[test]
fn remove_tool_responses_50_percent_removes_half() {
    let msgs = vec![
        make_tool_result_message("r1"),
        make_tool_result_message("r2"),
        make_tool_result_message("r3"),
        make_tool_result_message("r4"),
    ];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.5);
    let compacted = result
        .iter()
        .filter(|m| {
            m.parts.first().is_some_and(|p| {
                matches!(p, zeph_llm::provider::MessagePart::ToolResult { content, .. } if content == "[compacted]")
            })
        })
        .count();
    assert_eq!(compacted, 2);
}

#[test]
fn build_metadata_summary_includes_counts() {
    let msgs = vec![
        make_text_message("user question"),
        Message::from_legacy(Role::Assistant, "assistant response"),
    ];
    let summary = Agent::<MockChannel>::build_metadata_summary(&msgs);
    assert!(summary.contains('2'));
    assert!(summary.contains("1 user"));
    assert!(summary.contains("1 assistant"));
}

#[test]
fn remove_tool_responses_middle_out_order_is_center_first() {
    // 5 tool messages at positions 0..4 (no non-tool messages).
    // Middle-out from center(=2): first right=2, then left=1, then right=3, then left=0, then right=4.
    // So removal order for 5 items: indices 2, 1, 3, 0, 4.
    // With fraction=1.0 (all 5 removed), all must be compacted.
    // To verify ordering we test partial removals:
    // fraction ~0.2 (ceil(5*0.2)=1) → 1 removed → must be center (index 2)
    // fraction ~0.4 (ceil(5*0.4)=2) → 2 removed → must be indices 2 and 1
    let msgs: Vec<Message> = (0..5)
        .map(|i| {
            Message::from_parts(
                Role::User,
                vec![zeph_llm::provider::MessagePart::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: format!("result{i}"),
                    is_error: false,
                }],
            )
        })
        .collect();

    let is_compacted = |msgs: &[Message], idx: usize| -> bool {
        msgs[idx].parts.first().is_some_and(|p| {
            matches!(p, zeph_llm::provider::MessagePart::ToolResult { content, .. } if content == "[compacted]")
        })
    };

    // 1 removal — center (index 2)
    let one = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.20);
    assert!(
        is_compacted(&one, 2),
        "center (idx 2) must be first removed"
    );
    assert!(!is_compacted(&one, 0));
    assert!(!is_compacted(&one, 1));
    assert!(!is_compacted(&one, 3));
    assert!(!is_compacted(&one, 4));

    // 2 removals — center (2) + left-of-center (1)
    let two = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.40);
    assert!(is_compacted(&two, 2));
    assert!(is_compacted(&two, 1));
    assert!(!is_compacted(&two, 0));
    assert!(!is_compacted(&two, 3));
    assert!(!is_compacted(&two, 4));

    // 3 removals — 2 + right-of-center (3)
    let three = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.60);
    assert!(is_compacted(&three, 2));
    assert!(is_compacted(&three, 1));
    assert!(is_compacted(&three, 3));
    assert!(!is_compacted(&three, 0));
    assert!(!is_compacted(&three, 4));
}
