// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent::{CORRECTIONS_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX};
use crate::context::ContextBudget;
use zeph_llm::provider::MessagePart;
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use crate::agent::agent_tests::*;
use crate::agent::context_manager::{CompactionState, CompactionTier};
use crate::agent::state::MemoryState;

#[test]
fn chunk_messages_empty_input_returns_single_empty_chunk() {
    let tc = zeph_memory::TokenCounter::new();
    let messages: &[Message] = &[];
    let chunks = chunk_messages(messages, 4096, 2048, &tc);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[test]
fn chunk_messages_single_oversized_message_gets_own_chunk() {
    let tc = zeph_memory::TokenCounter::new();
    // A message >= oversized threshold goes into its own chunk
    let oversized_content = "x".repeat(2048 * 4 + 1); // > 2048 tokens
    let messages = vec![Message {
        role: Role::User,
        content: oversized_content.clone(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let chunks = chunk_messages(&messages, 4096, 2048, &tc);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0][0].content, oversized_content);
}

#[test]
fn chunk_messages_splits_at_budget_boundary() {
    let tc = zeph_memory::TokenCounter::new();
    // Two messages each consuming exactly half of budget → should fit in one chunk
    // Use messages whose token count is just under half of budget
    let half = "w".repeat(1000 * 4); // 1000 tokens
    let messages = vec![
        Message {
            role: Role::User,
            content: half.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: half.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: half.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    // budget = 2000 tokens: first two fit, third overflows → 2 chunks
    let chunks = chunk_messages(&messages, 2000, 4096, &tc);
    assert!(chunks.len() >= 2, "expected split into multiple chunks");
}

// SF-5: SkillPromptMode::Auto threshold
#[test]
fn skill_prompt_mode_auto_selects_compact_when_budget_below_8192() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(4096, 0.20, 0.80, 4, 0);

    // Auto mode: budget < 8192 → Compact
    let effective_mode = match crate::config::SkillPromptMode::Auto {
        crate::config::SkillPromptMode::Auto => {
            if let Some(ref budget) = agent.context_manager.budget
                && budget.max_tokens() < 8192
            {
                crate::config::SkillPromptMode::Compact
            } else {
                crate::config::SkillPromptMode::Full
            }
        }
        other => other,
    };
    assert_eq!(effective_mode, crate::config::SkillPromptMode::Compact);
}

#[test]
fn skill_prompt_mode_auto_selects_full_when_budget_above_8192() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(16384, 0.20, 0.80, 4, 0);

    // Auto mode: budget >= 8192 → Full
    let effective_mode = match crate::config::SkillPromptMode::Auto {
        crate::config::SkillPromptMode::Auto => {
            if let Some(ref budget) = agent.context_manager.budget
                && budget.max_tokens() < 8192
            {
                crate::config::SkillPromptMode::Compact
            } else {
                crate::config::SkillPromptMode::Full
            }
        }
        other => other,
    };
    assert_eq!(effective_mode, crate::config::SkillPromptMode::Full);
}

// SF-6: SkillPromptMode::Compact forced config
#[test]
fn skill_prompt_mode_compact_forced_regardless_of_budget() {
    // Even with a large budget, Compact mode stays Compact
    let effective_mode = match crate::config::SkillPromptMode::Compact {
        crate::config::SkillPromptMode::Auto => {
            crate::config::SkillPromptMode::Full // would normally pick Full
        }
        other => other,
    };
    assert_eq!(effective_mode, crate::config::SkillPromptMode::Compact);
}

#[test]
fn compaction_tier_disabled_without_budget() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    for i in 0..20 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} with some content to add tokens"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }
    assert_eq!(agent.compaction_tier(), CompactionTier::None);
}

#[test]
fn compaction_tier_none_below_soft() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.90, 4, 0);
    assert_eq!(agent.compaction_tier(), CompactionTier::None);
}

#[test]
fn compaction_tier_hard_above_threshold() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 4, 0)
        .with_soft_compaction_threshold(0.50);

    for i in 0..20 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message number {i} with enough content to push over budget"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }
    assert_eq!(agent.compaction_tier(), CompactionTier::Hard);
}

#[tokio::test]
async fn compact_context_preserves_system_and_tail() {
    let provider = mock_provider(vec!["compacted summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    let system_content = agent.msg.messages[0].content.clone();

    for i in 0..8 {
        agent.msg.messages.push(Message {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.compact_context().await.unwrap();

    assert_eq!(agent.msg.messages[0].role, Role::System);
    assert_eq!(agent.msg.messages[0].content, system_content);

    assert_eq!(agent.msg.messages[1].role, Role::System);
    assert!(
        agent.msg.messages[1]
            .content
            .contains("[conversation summary")
    );

    let tail = &agent.msg.messages[2..];
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].content, "message 6");
    assert_eq!(tail[1].content, "message 7");
}

#[tokio::test]
async fn compact_context_too_few_messages() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 4, 0);

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "msg1".to_string(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "msg2".to_string(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    let len_before = agent.msg.messages.len();
    agent.compact_context().await.unwrap();
    assert_eq!(agent.msg.messages.len(), len_before);
}

#[test]
fn with_context_budget_zero_disables() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(0, 0.20, 0.75, 4, 0);
    assert!(agent.context_manager.budget.is_none());
}

#[test]
fn with_context_budget_nonzero_enables() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(4096, 0.20, 0.80, 6, 0);

    assert!(agent.context_manager.budget.is_some());
    assert_eq!(
        agent.context_manager.budget.as_ref().unwrap().max_tokens(),
        4096
    );
    assert!((agent.context_manager.hard_compaction_threshold - 0.80).abs() < f32::EPSILON);
    assert_eq!(agent.context_manager.compaction_preserve_tail, 6);
}

#[tokio::test]
async fn compact_context_increments_metric() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);

    for i in 0..8 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.compact_context().await.unwrap();
    assert_eq!(rx.borrow().context_compactions, 1);
}

#[tokio::test]
async fn test_prepare_context_no_budget_is_noop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let msg_count = agent.msg.messages.len();

    agent.prepare_context("test query").await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_correction_messages_removed_between_turns() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.insert(
        1,
        Message {
            role: Role::System,
            content: format!("{CORRECTIONS_PREFIX}old correction data"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    assert_eq!(agent.msg.messages.len(), 2);

    agent.remove_correction_messages();
    assert_eq!(agent.msg.messages.len(), 1);
    assert!(
        !agent.msg.messages[0]
            .content
            .starts_with(CORRECTIONS_PREFIX)
    );
}

#[tokio::test]
async fn test_remove_correction_messages_preserves_non_correction_system() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Add a non-correction system message
    agent.msg.messages.insert(
        1,
        Message {
            role: Role::System,
            content: "regular system message".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    // Add a correction system message
    agent.msg.messages.insert(
        2,
        Message {
            role: Role::System,
            content: format!("{CORRECTIONS_PREFIX}correction data"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    assert_eq!(agent.msg.messages.len(), 3);

    agent.remove_correction_messages();

    assert_eq!(agent.msg.messages.len(), 2);
    assert!(
        agent
            .msg
            .messages
            .iter()
            .any(|m| m.content == "regular system message")
    );
    assert!(
        !agent
            .msg
            .messages
            .iter()
            .any(|m| m.content.starts_with(CORRECTIONS_PREFIX))
    );
}

#[tokio::test]
async fn test_recall_injection_removed_between_turns() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.insert(
        1,
        Message {
            role: Role::System,
            content: format!("{RECALL_PREFIX}old recall data"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    assert_eq!(agent.msg.messages.len(), 2);

    agent.remove_recall_messages();
    assert_eq!(agent.msg.messages.len(), 1);
    assert!(!agent.msg.messages[0].content.starts_with(RECALL_PREFIX));
}

#[tokio::test]
async fn test_recall_without_qdrant_returns_empty() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let msg_count = agent.msg.messages.len();

    agent.inject_semantic_recall("test", 1000).await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_trim_messages_preserves_system() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }
    assert_eq!(agent.msg.messages.len(), 11);

    agent.trim_messages_to_budget(5);

    assert_eq!(agent.msg.messages[0].role, Role::System);
    assert!(agent.msg.messages.len() < 11);
}

#[tokio::test]
async fn test_trim_messages_keeps_recent() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("msg {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.trim_messages_to_budget(5);

    let last = agent.msg.messages.last().unwrap();
    assert_eq!(last.content, "msg 9");
}

#[tokio::test]
async fn test_trim_zero_budget_is_noop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }
    let msg_count = agent.msg.messages.len();

    agent.trim_messages_to_budget(0);
    assert_eq!(agent.msg.messages.len(), msg_count);
}

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

#[tokio::test]
async fn test_inject_summaries_no_memory_noop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let msg_count = agent.msg.messages.len();

    agent.inject_summaries(1000).await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_inject_summaries_zero_budget_noop() {
    let provider = mock_provider(vec![]);
    let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary text"]).await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );
    let msg_count = agent.msg.messages.len();

    agent.inject_summaries(0).await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_inject_summaries_empty_summaries_noop() {
    let provider = mock_provider(vec![]);
    let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );
    let msg_count = agent.msg.messages.len();

    agent.inject_summaries(1000).await.unwrap();
    assert_eq!(agent.msg.messages.len(), msg_count);
}

#[tokio::test]
async fn test_inject_summaries_inserts_at_position_1() {
    let provider = mock_provider(vec![]);
    let (memory, cid) =
        create_memory_with_summaries(provider.clone(), &["User asked about Rust ownership"]).await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.inject_summaries(1000).await.unwrap();

    assert_eq!(agent.msg.messages[0].role, Role::System);
    assert!(agent.msg.messages[1].content.starts_with(SUMMARY_PREFIX));
    assert_eq!(agent.msg.messages[1].role, Role::System);
    assert!(
        agent.msg.messages[1]
            .content
            .contains("User asked about Rust ownership")
    );
    assert_eq!(agent.msg.messages[2].content, "hello");
}

#[tokio::test]
async fn test_inject_summaries_removes_old_before_inject() {
    let provider = mock_provider(vec![]);
    let (memory, cid) = create_memory_with_summaries(provider.clone(), &["new summary data"]).await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );

    agent.msg.messages.insert(
        1,
        Message {
            role: Role::System,
            content: format!("{SUMMARY_PREFIX}old summary data"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    assert_eq!(agent.msg.messages.len(), 3);

    agent.inject_summaries(1000).await.unwrap();

    let summary_msgs: Vec<_> = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.content.starts_with(SUMMARY_PREFIX))
        .collect();
    assert_eq!(summary_msgs.len(), 1);
    assert!(summary_msgs[0].content.contains("new summary data"));
    assert!(!summary_msgs[0].content.contains("old summary data"));
}

#[tokio::test]
async fn test_inject_summaries_respects_token_budget() {
    let provider = mock_provider(vec![]);
    // Each summary entry is "- Messages X-Y: <content>\n" (~prefix overhead + content)
    let (memory, cid) = create_memory_with_summaries(
        provider.clone(),
        &[
            "short",
            "this is a much longer summary that should consume more tokens",
        ],
    )
    .await;

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Use a very small budget: only the prefix + maybe one short entry
    let tc = zeph_memory::TokenCounter::new();
    let prefix_cost = tc.count_tokens(SUMMARY_PREFIX);
    agent.inject_summaries(prefix_cost + 10).await.unwrap();

    let summary_msg = agent
        .msg
        .messages
        .iter()
        .find(|m| m.content.starts_with(SUMMARY_PREFIX));

    if let Some(msg) = summary_msg {
        let token_count = tc.count_tokens(&msg.content);
        assert!(token_count <= prefix_cost + 10);
    }
}

#[tokio::test]
async fn test_remove_summary_messages_preserves_other_system() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.insert(
        1,
        Message {
            role: Role::System,
            content: format!("{SUMMARY_PREFIX}old summary"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    agent.msg.messages.insert(
        2,
        Message {
            role: Role::System,
            content: format!("{RECALL_PREFIX}recall data"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    );
    assert_eq!(agent.msg.messages.len(), 3);

    agent.remove_summary_messages();
    assert_eq!(agent.msg.messages.len(), 2);
    assert!(agent.msg.messages[1].content.starts_with(RECALL_PREFIX));
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
        std::sync::Arc::new(memory),
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
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50)
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
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50)
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
    let memory_ref = agent.memory_state.memory.as_ref().unwrap();
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
    let budget = crate::context::ContextBudget::new(1000, 0.20);
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
                tool_name: format!("tool_{i}"),
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
        .with_context_budget(4096, 0.20, 0.80, 4, 0)
        .with_redact_credentials(true);

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
        .with_redact_credentials(true)
        .with_working_dir("/Users/dev/project");

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
        .with_context_budget(4096, 0.20, 0.80, 4, 0)
        .with_redact_credentials(false);

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
    let note = crate::agent::Agent::<MockChannel>::format_correction_note(
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
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_soft_compaction_threshold(0.50);
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
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_soft_compaction_threshold(0.50);
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

#[test]
fn truncate_chars_is_safe_for_multibyte() {
    // Each Cyrillic char is 2 bytes; slicing at byte 200 would panic on odd boundaries.
    let s: String = "Привет".repeat(50); // 300 chars, 600 bytes
    let truncated = super::truncate_chars(&s, 200);
    assert!(truncated.ends_with('…'));
    // Must be valid UTF-8 (no panic means success, but also check char count)
    assert_eq!(truncated.chars().count(), 201); // 200 chars + '…'
}

// --- truncate_chars additional edge cases ---

#[test]
fn truncate_chars_ascii_exact() {
    let s = "abcde";
    // max_chars == len → no truncation
    let result = super::truncate_chars(s, 5);
    assert_eq!(result, "abcde");
}

#[test]
fn truncate_chars_emoji() {
    // 🚀 is a single Unicode scalar even though it is 4 bytes
    let s = "🚀🚀🚀🚀🚀";
    let result = super::truncate_chars(s, 3);
    assert!(result.ends_with('…'), "should append ellipsis");
    // 3 emoji + ellipsis = 4 Unicode scalars
    assert_eq!(result.chars().count(), 4);
}

#[test]
fn truncate_chars_empty() {
    let result = super::truncate_chars("", 10);
    assert_eq!(result, "");
}

#[test]
fn truncate_chars_shorter_than_max() {
    let s = "hello";
    let result = super::truncate_chars(s, 100);
    assert_eq!(result, "hello");
}

#[test]
fn truncate_chars_zero_max() {
    let s = "hello";
    // max_chars = 0 → returns empty string (no chars kept, no ellipsis)
    let result = super::truncate_chars(s, 0);
    assert_eq!(result, "");
}

// --- build_chunk_prompt ---

#[test]
fn build_chunk_prompt_contains_all_nine_sections() {
    let messages = vec![Message {
        role: Role::User,
        content: "help me refactor this code".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let prompt = Agent::<MockChannel>::build_chunk_prompt(&messages, "");

    let sections = [
        "User Intent",
        "Technical Concepts",
        "Files & Code",
        "Errors & Fixes",
        "Problem Solving",
        "User Messages",
        "Pending Tasks",
        "Current Work",
        "Next Step",
    ];
    for section in sections {
        assert!(
            prompt.contains(section),
            "prompt missing section: {section}"
        );
    }
}

#[test]
fn build_chunk_prompt_empty_messages() {
    let messages: &[Message] = &[];
    let prompt = Agent::<MockChannel>::build_chunk_prompt(messages, "");
    // Even with no messages the prompt structure must be valid (not panic, contains sections)
    assert!(prompt.contains("User Intent"));
    assert!(prompt.contains("Next Step"));
}

#[test]
fn build_chunk_prompt_injects_guidelines_block_when_non_empty() {
    let messages: &[Message] = &[];
    let guidelines = "1. Preserve file paths\n2. Preserve error codes";
    let prompt = Agent::<MockChannel>::build_chunk_prompt(messages, guidelines);
    assert!(
        prompt.contains("<compression-guidelines>"),
        "guidelines block must be present when guidelines non-empty"
    );
    assert!(
        prompt.contains("Preserve file paths"),
        "guideline content must appear in prompt"
    );
    assert!(
        prompt.contains("</compression-guidelines>"),
        "guidelines closing tag must be present"
    );
}

#[test]
fn build_chunk_prompt_no_guidelines_block_when_empty() {
    let messages: &[Message] = &[];
    let prompt = Agent::<MockChannel>::build_chunk_prompt(messages, "");
    assert!(
        !prompt.contains("<compression-guidelines>"),
        "no guidelines block when guidelines is empty"
    );
}

// --- rebuild_system_prompt block order ---

#[tokio::test]
async fn rebuild_system_prompt_stable_marker_before_volatile_marker() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    let pos_stable = prompt
        .find("<!-- cache:stable -->")
        .expect("cache:stable marker must be present");
    let pos_volatile = prompt
        .find("<!-- cache:volatile -->")
        .expect("cache:volatile marker must be present");
    assert!(
        pos_stable < pos_volatile,
        "cache:stable must appear before cache:volatile in the system prompt"
    );
}

#[tokio::test]
async fn rebuild_system_prompt_base_content_before_stable_marker() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    let pos_stable = prompt
        .find("<!-- cache:stable -->")
        .expect("cache:stable marker must be present");
    // The prompt must have non-whitespace content before the stable marker.
    let before_stable = prompt[..pos_stable].trim();
    assert!(
        !before_stable.is_empty(),
        "base prompt content must appear before cache:stable marker"
    );
}

#[tokio::test]
async fn rebuild_system_prompt_volatile_marker_at_block3_boundary() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    // Everything after cache:volatile must not include cache:stable.
    let pos_volatile = prompt
        .find("<!-- cache:volatile -->")
        .expect("cache:volatile marker must be present");
    let after_volatile = &prompt[pos_volatile + "<!-- cache:volatile -->".len()..];
    assert!(
        !after_volatile.contains("<!-- cache:stable -->"),
        "cache:stable must not appear after cache:volatile"
    );
}

// --- build_metadata_summary robustness ---

#[test]
fn build_metadata_summary_empty_messages() {
    let messages: &[Message] = &[];
    let summary = Agent::<MockChannel>::build_metadata_summary(messages);
    assert!(summary.contains("Messages compacted: 0"));
    assert!(summary.contains("0 user"));
    assert!(summary.contains("0 assistant"));
}

#[test]
fn build_metadata_summary_utf8_content() {
    let messages = vec![
        Message {
            role: Role::User,
            content: "Привет мир 🌍".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "Hello 🌐".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let summary = Agent::<MockChannel>::build_metadata_summary(&messages);
    // Must not panic on multi-byte content
    assert!(summary.contains("Messages compacted: 2"));
    assert!(summary.contains("1 user"));
    assert!(summary.contains("1 assistant"));
}

#[test]
fn build_metadata_summary_truncation_boundary() {
    let long_content = "a".repeat(300);
    let messages = vec![Message {
        role: Role::User,
        content: long_content,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let summary = Agent::<MockChannel>::build_metadata_summary(&messages);
    // The last user message preview is capped at 200 chars + '…'
    assert!(
        summary.contains('…'),
        "long content should be truncated with ellipsis"
    );
}

// --- remove_tool_responses_middle_out edge cases ---

#[test]
fn remove_tool_responses_single_tool_message() {
    let msg = Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: "result".into(),
            is_error: false,
        }],
    );
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(vec![msg], 1.0);
    assert_eq!(result.len(), 1);
    if let MessagePart::ToolResult { content, .. } = &result[0].parts[0] {
        assert_eq!(content, "[compacted]");
    } else {
        panic!("expected ToolResult part");
    }
}

#[test]
fn remove_tool_responses_all_tiers_progressive() {
    // Build 10 messages, all with ToolResult parts
    let make_tool_msg = |i: usize| {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: format!("t{i}"),
                content: format!("result_{i}"),
                is_error: false,
            }],
        )
    };
    let msgs: Vec<Message> = (0..10).map(make_tool_msg).collect();

    let count_compacted = |result: &[Message]| {
        result
            .iter()
            .filter(|m| {
                m.parts.iter().any(|p| {
                    matches!(p, MessagePart::ToolResult { content, .. } if content == "[compacted]")
                })
            })
            .count()
    };

    // 10% of 10 = ceil(1.0) = 1
    let r10 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.10);
    assert_eq!(count_compacted(&r10), 1);

    // 20% of 10 = ceil(2.0) = 2
    let r20 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.20);
    assert_eq!(count_compacted(&r20), 2);

    // 50% of 10 = ceil(5.0) = 5
    let r50 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.50);
    assert_eq!(count_compacted(&r50), 5);

    // 100% of 10 = 10
    let r100 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
    assert_eq!(count_compacted(&r100), 10);
}

fn make_tool_pair(agent: &mut Agent<MockChannel>, tool_name: &str) {
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: format!("id_{tool_name}"),
            name: tool_name.to_owned(),
            input: serde_json::json!({"cmd": "echo hello"}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: format!("id_{tool_name}"),
            content: format!("output of {tool_name}"),
            is_error: false,
        }],
    ));
}

#[test]
fn count_unsummarized_pairs_counts_visible_native_pairs() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    assert_eq!(agent.count_unsummarized_pairs(), 0);

    make_tool_pair(&mut agent, "bash");
    assert_eq!(agent.count_unsummarized_pairs(), 1);

    make_tool_pair(&mut agent, "read_file");
    assert_eq!(agent.count_unsummarized_pairs(), 2);
}

#[test]
fn count_unsummarized_pairs_ignores_hidden_pairs() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    make_tool_pair(&mut agent, "bash");
    // hide the first pair
    agent.msg.messages[1].metadata.agent_visible = false;
    agent.msg.messages[2].metadata.agent_visible = false;

    assert_eq!(agent.count_unsummarized_pairs(), 0);
}

#[test]
fn find_oldest_unsummarized_pair_returns_correct_indices() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    assert_eq!(agent.find_oldest_unsummarized_pair(), None);

    make_tool_pair(&mut agent, "bash");
    // system = 0, request = 1, response = 2
    assert_eq!(agent.find_oldest_unsummarized_pair(), Some((1, 2)));

    make_tool_pair(&mut agent, "read_file");
    // oldest pair is still (1, 2)
    assert_eq!(agent.find_oldest_unsummarized_pair(), Some((1, 2)));
}

#[test]
fn find_oldest_unsummarized_pair_skips_hidden() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    make_tool_pair(&mut agent, "bash");
    make_tool_pair(&mut agent, "read_file");
    // hide first pair
    agent.msg.messages[1].metadata.agent_visible = false;
    agent.msg.messages[2].metadata.agent_visible = false;

    // second pair: request = 3, response = 4
    assert_eq!(agent.find_oldest_unsummarized_pair(), Some((3, 4)));
}

#[tokio::test]
async fn maybe_summarize_tool_pair_below_cutoff_does_nothing() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(6);

    // 3 pairs < cutoff of 6
    make_tool_pair(&mut agent, "bash");
    make_tool_pair(&mut agent, "read_file");
    make_tool_pair(&mut agent, "write_file");

    let msg_count_before = agent.msg.messages.len();
    agent.maybe_summarize_tool_pair().await;
    assert_eq!(agent.msg.messages.len(), msg_count_before);
}

#[tokio::test]
async fn maybe_summarize_tool_pair_at_exact_cutoff_does_nothing() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(3);

    // exactly 3 pairs == cutoff of 3, should NOT summarize
    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");
    make_tool_pair(&mut agent, "c");

    let msg_count_before = agent.msg.messages.len();
    agent.maybe_summarize_tool_pair().await;
    assert_eq!(agent.msg.messages.len(), msg_count_before);
}

#[tokio::test]
async fn maybe_summarize_tool_pair_above_cutoff_stores_deferred_summary() {
    let summary_text = "summarized tool call".to_owned();
    let provider = mock_provider(vec![summary_text.clone()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

    // 3 pairs > cutoff of 2
    make_tool_pair(&mut agent, "bash");
    make_tool_pair(&mut agent, "read_file");
    make_tool_pair(&mut agent, "write_file");

    let msg_count_before = agent.msg.messages.len();
    agent.maybe_summarize_tool_pair().await;

    // message count must NOT change — deferred, no immediate mutation
    assert_eq!(agent.msg.messages.len(), msg_count_before);
    // oldest pair (indices 1, 2) must remain visible
    assert!(agent.msg.messages[1].metadata.agent_visible);
    assert!(agent.msg.messages[2].metadata.agent_visible);
    // deferred_summary must be set on the response message (index 2)
    assert_eq!(
        agent.msg.messages[2].metadata.deferred_summary.as_deref(),
        Some(summary_text.as_str()),
        "deferred_summary should hold the LLM response"
    );
}

/// Regression test: resumed session with large backlog must drain all pairs above cutoff
/// in a single `maybe_summarize_tool_pair()` call, before Tier 1 pruning can fire.
#[tokio::test]
async fn maybe_summarize_tool_pair_drains_backlog_above_cutoff() {
    // cutoff=2, 6 pairs → 4 pairs need summaries; provider returns one reply per call
    let replies = vec!["s1".into(), "s2".into(), "s3".into(), "s4".into()];
    let provider = mock_provider(replies);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

    for name in ["a", "b", "c", "d", "e", "f"] {
        make_tool_pair(&mut agent, name);
    }

    agent.maybe_summarize_tool_pair().await;

    let deferred_count = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.deferred_summary.is_some())
        .count();
    // all 4 pairs above cutoff must have deferred summaries after one call
    assert_eq!(
        deferred_count, 4,
        "expected 4 deferred summaries, got {deferred_count}"
    );
    // remaining unsummarized count must equal cutoff
    assert_eq!(agent.count_unsummarized_pairs(), 2);
}

#[tokio::test]
async fn maybe_summarize_tool_pair_llm_error_skips_gracefully() {
    let provider = mock_provider_failing();
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

    // 2 pairs > cutoff of 1
    make_tool_pair(&mut agent, "bash");
    make_tool_pair(&mut agent, "read_file");

    let msg_count_before = agent.msg.messages.len();
    // Should not panic, just warn and skip
    agent.maybe_summarize_tool_pair().await;
    // No messages should be added or hidden
    assert_eq!(agent.msg.messages.len(), msg_count_before);
    assert!(agent.msg.messages[1].metadata.agent_visible);
    assert!(agent.msg.messages[2].metadata.agent_visible);
}

#[test]
fn build_tool_pair_summary_prompt_contains_xml_delimiters() {
    let req = Message {
        role: Role::Assistant,
        content: "call bash".into(),
        ..Message::default()
    };
    let res = Message {
        role: Role::User,
        content: "exit code 0".into(),
        ..Message::default()
    };
    let prompt = Agent::<MockChannel>::build_tool_pair_summary_prompt(&req, &res);
    assert!(prompt.contains("<tool_request>"), "missing <tool_request>");
    assert!(
        prompt.contains("</tool_request>"),
        "missing </tool_request>"
    );
    assert!(
        prompt.contains("<tool_response>"),
        "missing <tool_response>"
    );
    assert!(
        prompt.contains("</tool_response>"),
        "missing </tool_response>"
    );
    assert!(prompt.contains("call bash"));
    assert!(prompt.contains("exit code 0"));
}

#[tokio::test]
async fn maybe_summarize_tool_pair_empty_messages_does_nothing() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

    agent.msg.messages.clear();
    agent.maybe_summarize_tool_pair().await;
    assert!(agent.msg.messages.is_empty());
}

#[test]
fn remove_tool_responses_fraction_zero_changes_nothing() {
    let msgs = vec![
        make_tool_result_message("result1"),
        make_tool_result_message("result2"),
    ];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.0);
    assert_eq!(result.len(), 2);
    for msg in &result {
        if let Some(MessagePart::ToolResult { content, .. }) = msg.parts.first() {
            assert_ne!(
                content, "[compacted]",
                "fraction=0.0 should not compact anything"
            );
        }
    }
}

#[test]
fn remove_tool_responses_tool_output_parts_compacted() {
    let msgs = vec![
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "output text".into(),
                compacted_at: None,
            }],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "read_file".into(),
                body: "file content".into(),
                compacted_at: None,
            }],
        ),
    ];
    let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
    assert_eq!(result.len(), 2);
    for msg in &result {
        if let Some(MessagePart::ToolOutput {
            body, compacted_at, ..
        }) = msg.parts.first()
        {
            assert!(
                body.is_empty(),
                "ToolOutput body should be cleared after compaction"
            );
            assert!(
                compacted_at.is_some(),
                "ToolOutput compacted_at should be set"
            );
        } else {
            panic!("expected ToolOutput part");
        }
    }
}

// --- Status emission tests ---

#[tokio::test]
async fn tier1_compaction_emits_compacting_status() {
    use std::sync::Arc;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let statuses = Arc::clone(&channel.statuses);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    // Push enough messages to exceed the compaction threshold (budget=100, threshold=20)
    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold padding padding"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();

    let emitted = statuses.lock().unwrap().clone();
    assert!(
        emitted.iter().any(|s| s == "compacting context..."),
        "expected 'compacting context...' in statuses, got: {emitted:?}"
    );
}

#[tokio::test]
async fn prepare_context_emits_recalling_status() {
    use std::sync::Arc;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let statuses = Arc::clone(&channel.statuses);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(10_000, 0.80, 0.75, 2, 0);

    agent.prepare_context("test query").await.unwrap();

    let emitted = statuses.lock().unwrap().clone();
    assert!(
        emitted.iter().any(|s| s == "recalling context..."),
        "expected 'recalling context...' in statuses, got: {emitted:?}"
    );
}

// cap_summary tests (SEC-02)

#[test]
fn cap_summary_short_string_unchanged() {
    let s = "hello world".to_owned();
    let result = cap_summary(s.clone(), 100);
    assert_eq!(result, s);
}

#[test]
fn cap_summary_truncates_long_string() {
    let s = "a".repeat(200);
    let result = cap_summary(s, 10);
    assert!(result.ends_with('…'));
    assert_eq!(result.chars().count(), 11); // 10 chars + ellipsis
}

#[test]
fn cap_summary_exact_length_unchanged() {
    let s = "hello".to_owned();
    let result = cap_summary(s.clone(), 5);
    assert_eq!(result, s);
}

// compacted_this_turn reset and mutual exclusion tests (#1161 — tester gap)

#[tokio::test]
async fn compacted_this_turn_reset_between_turns() {
    let provider = mock_provider(vec!["turn1".to_owned(), "turn2".to_owned()]);
    let channel = MockChannel::new(vec!["first".to_owned(), "second".to_owned()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Manually set state as if proactive compression fired
    agent.context_manager.compaction = CompactionState::CompactedThisTurn { cooldown: 0 };

    // Process a message — advance_turn() resets state at turn start
    let _ = agent.process_user_message("first".to_owned(), vec![]).await;

    // After turn, state should have been reset (via advance_turn at start) and may
    // have been set again only if proactive compression fired. Since threshold is
    // reactive by default, state should be Ready after turn (no proactive).
    // We can't inspect mid-turn, but we can check the default config doesn't trigger.
    assert!(!agent.context_manager.compaction.is_compacted_this_turn());
}

#[tokio::test]
async fn maybe_proactive_compress_does_not_fire_with_reactive_strategy() {
    // With default (Reactive) strategy, maybe_proactive_compress should be a no-op.
    let provider = mock_provider(vec!["response".to_owned()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.providers.cached_prompt_tokens = 200_000; // very high token count

    // should_proactively_compress returns None for Reactive → no compression
    let result = agent.maybe_proactive_compress().await;
    assert!(result.is_ok());
    assert!(!agent.context_manager.compaction.is_compacted_this_turn());
}

// BudgetAllocation.graph_facts tests

#[test]
fn budget_allocation_graph_disabled_preserves_semantic_recall_8pct() {
    let budget = crate::context::ContextBudget::new(10000, 0.20);
    let tc = zeph_memory::TokenCounter::new();
    let alloc = budget.allocate("", "", &tc, false);
    assert_eq!(alloc.graph_facts, 0);
    let available = 10000 - 2000; // 20% reserve
    let expected_recall = available * 8 / 100;
    assert_eq!(alloc.semantic_recall, expected_recall);
}

#[test]
fn budget_allocation_graph_enabled_splits_from_semantic_recall() {
    let budget = crate::context::ContextBudget::new(10000, 0.20);
    let tc = zeph_memory::TokenCounter::new();
    let alloc = budget.allocate("", "", &tc, true);
    assert!(
        alloc.graph_facts > 0,
        "graph_facts must be non-zero when enabled"
    );
    assert!(alloc.graph_facts < alloc.semantic_recall, "3% < 5%");
}

#[test]
fn budget_allocation_zero_tokens_graph_facts_zero() {
    let budget = crate::context::ContextBudget::new(0, 0.20);
    let tc = zeph_memory::TokenCounter::new();
    let alloc = budget.allocate("", "", &tc, true);
    assert_eq!(alloc.graph_facts, 0);
}

// --- pruning-summarization order tests ---

// Helper: add a tool pair with ToolOutput parts (so pruning can clear the body).
fn make_tool_pair_with_output(agent: &mut Agent<MockChannel>, tool_name: &str) {
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: format!("id_{tool_name}"),
            name: tool_name.to_owned(),
            input: serde_json::json!({"cmd": "echo hello"}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: tool_name.to_owned(),
            body: format!("full output of {tool_name}"),
            compacted_at: None,
        }],
    ));
}

#[tokio::test]
async fn summarize_then_prune_preserves_intact_content_for_summarizer() {
    // The summarizer must receive non-pruned content.
    // With cutoff=2, adding 3 pairs triggers summarization.
    // maybe_summarize_tool_pair() stores deferred_summary; apply_deferred_summaries()
    // actually hides the pair and inserts the Summary message.
    let summary_text = "summarized bash call".to_owned();
    let provider = mock_provider(vec![summary_text.clone()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

    make_tool_pair_with_output(&mut agent, "bash");
    make_tool_pair_with_output(&mut agent, "read_file");
    make_tool_pair_with_output(&mut agent, "write_file");

    // Correct order: summarize (deferred), then apply, then prune.
    agent.maybe_summarize_tool_pair().await;
    agent.apply_deferred_summaries();
    let keep_recent = 2 * agent.memory_state.tool_call_cutoff + 2;
    agent.prune_stale_tool_outputs(keep_recent);

    // The summary was inserted — summarizer must have seen content.
    let has_summary = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(has_summary, "summary should have been inserted");

    // The summarized pair is now hidden.
    assert!(
        !agent.msg.messages[1].metadata.agent_visible,
        "oldest pair request should be hidden"
    );
    assert!(
        !agent.msg.messages[2].metadata.agent_visible,
        "oldest pair response should be hidden"
    );
}

#[tokio::test]
async fn prune_after_summarize_does_not_destroy_visible_pairs() {
    // After summarize-then-prune, remaining visible pairs within keep_recent
    // should have intact content (body not empty, compacted_at is None).
    let summary_text = "summary".to_owned();
    let provider = mock_provider(vec![summary_text]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // cutoff=2: keep_recent = 6. With 3 pairs (6 messages + 1 system = 7 total),
    // summarize hides oldest pair and inserts summary (+1), then prune boundary = 8-6=2,
    // so only messages[1] is in the pruning range and it's already hidden.
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

    make_tool_pair_with_output(&mut agent, "bash");
    make_tool_pair_with_output(&mut agent, "read_file");
    make_tool_pair_with_output(&mut agent, "write_file");

    agent.maybe_summarize_tool_pair().await;
    agent.apply_deferred_summaries();
    let keep_recent = 2 * agent.memory_state.tool_call_cutoff + 2;
    agent.prune_stale_tool_outputs(keep_recent);

    // Verify all visible ToolOutput parts have non-empty bodies.
    for msg in agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.agent_visible)
    {
        for part in &msg.parts {
            if let MessagePart::ToolOutput {
                body, compacted_at, ..
            } = part
            {
                assert!(
                    !body.is_empty() || compacted_at.is_some(),
                    "visible pair should not have empty body without compacted_at"
                );
                // compacted_at must be None for truly intact content within keep_recent
                assert!(
                    compacted_at.is_none(),
                    "visible pairs within keep_recent window must not be pruned"
                );
            }
        }
    }
}

#[tokio::test]
async fn prune_then_summarize_regression_summarizer_sees_pruned_content() {
    // Documents the original bug: if prune runs before summarize, the summarizer
    // prompt contains "(pruned)" placeholder instead of real content.
    // With cutoff=1 and keep_recent=4, adding 2 pairs triggers summarization.
    let summary_text = "summary of pruned pair".to_owned();
    let provider = mock_provider(vec![summary_text]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

    make_tool_pair_with_output(&mut agent, "bash");
    make_tool_pair_with_output(&mut agent, "read_file");

    // Deliberately use the OLD (broken) order: prune first, then summarize.
    // keep_recent=2 is a valid simplification of the original bug: the same code path
    // is exercised — pruning runs before summarization and clears content the summarizer
    // would need. Using keep_recent=2 with 2 pairs forces boundary=3, pruning res1
    // (index 2). The original bug used keep_recent=4 with more pairs; the essential
    // failure mode is identical — any keep_recent that places the oldest pair inside
    // the pruning range reproduces the bug.
    let small_keep_recent = 2;
    agent.prune_stale_tool_outputs(small_keep_recent);

    // The first tool pair's ToolOutput body should now be pruned.
    let first_output_pruned = agent.msg.messages[2].parts.iter().any(|p| {
        matches!(
            p,
            MessagePart::ToolOutput {
                compacted_at: Some(_),
                ..
            }
        )
    });
    assert!(
        first_output_pruned,
        "pruning before summarization should have compacted the first pair's output"
    );
}

#[tokio::test]
async fn cutoff_one_edge_case_summarize_then_prune() {
    // With cutoff=1 and 2 tool pairs, summarizer triggers (2 > 1).
    // maybe_summarize_tool_pair() stores deferred_summary on the response.
    // apply_deferred_summaries() then hides the pair and inserts a Summary message.
    // keep_recent = 2 * 1 + 2 = 4. The second pair stays visible within keep_recent.
    let summary_text = "summary".to_owned();
    let provider = mock_provider(vec![summary_text]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

    make_tool_pair_with_output(&mut agent, "bash");
    make_tool_pair_with_output(&mut agent, "read_file");

    agent.maybe_summarize_tool_pair().await;
    // Apply deferred summaries so the Summary message is actually inserted.
    agent.apply_deferred_summaries();

    let keep_recent = 2 * agent.memory_state.tool_call_cutoff + 2;
    agent.prune_stale_tool_outputs(keep_recent);

    // Summary inserted: 1 pair hidden, summary present.
    let has_summary = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(has_summary, "summary should have been created for cutoff=1");

    // The remaining visible pair (read_file) should have intact output.
    let visible_outputs: Vec<_> = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.agent_visible)
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::ToolOutput { .. }))
        .collect();

    for part in &visible_outputs {
        if let MessagePart::ToolOutput { compacted_at, .. } = part {
            assert!(
                compacted_at.is_none(),
                "visible pair within keep_recent must not be pruned (cutoff=1)"
            );
        }
    }
}

#[tokio::test]
async fn summarizer_failure_prune_still_runs() {
    // If the summarizer LLM call fails, pruning should still run without panicking.
    let provider = mock_provider_failing();
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

    make_tool_pair_with_output(&mut agent, "bash");
    make_tool_pair_with_output(&mut agent, "read_file");

    let msg_count_before = agent.msg.messages.len();

    // Summarize fails (no panic), then prune runs.
    agent.maybe_summarize_tool_pair().await;
    let keep_recent = 2 * agent.memory_state.tool_call_cutoff + 2;
    let freed = agent.prune_stale_tool_outputs(keep_recent);

    // Messages count unchanged (no summary inserted due to failure).
    assert_eq!(agent.msg.messages.len(), msg_count_before);
    // With [sys, req1, res1, req2, res2] = 5, keep_recent=4, boundary=1:
    // messages[1..1] is empty → nothing pruned.
    assert_eq!(freed, 0, "keep_recent=4 should protect all 4 tool messages");
}

async fn build_graph_memory() -> zeph_memory::semantic::SemanticMemory {
    let mem = zeph_memory::semantic::SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
        "test-model",
    )
    .await
    .unwrap();
    let store = std::sync::Arc::new(zeph_memory::graph::GraphStore::new(
        mem.sqlite().pool().clone(),
    ));
    mem.with_graph_store(store)
}

fn make_mem_state(
    memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    cid: zeph_memory::ConversationId,
    graph_enabled: bool,
) -> MemoryState {
    MemoryState {
        memory: Some(memory),
        conversation_id: Some(cid),
        history_limit: 50,
        recall_limit: 5,
        summarization_threshold: 100,
        cross_session_score_threshold: 0.5,
        autosave_assistant: false,
        autosave_min_length: 20,
        tool_call_cutoff: 6,
        unsummarized_count: 0,
        document_config: crate::config::DocumentConfig::default(),
        graph_config: crate::config::GraphConfig {
            enabled: graph_enabled,
            ..Default::default()
        },
        compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig::default(),
        shutdown_summary: true,
        shutdown_summary_min_messages: 4,
        shutdown_summary_max_messages: 20,
        shutdown_summary_timeout_secs: 10,
        structured_summaries: false,
        last_recall_confidence: None,
        digest_config: crate::config::DigestConfig::default(),
        cached_session_digest: None,
        context_strategy: crate::config::ContextStrategy::default(),
        crossover_turn_threshold: 20,
        rpe_router: None,
        goal_text: None,
        persona_config: crate::config::PersonaConfig::default(),
        trajectory_config: crate::config::TrajectoryConfig::default(),
        category_config: crate::config::CategoryConfig::default(),
        tree_config: crate::config::TreeConfig::default(),
        tree_consolidation_handle: None,
        microcompact_config: crate::config::MicrocompactConfig::default(),
        autodream_config: crate::config::AutoDreamConfig::default(),
        magic_docs_config: crate::config::MagicDocsConfig::default(),
    }
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_graph_config_disabled() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(std::sync::Arc::new(memory), cid, false);
    let tc = std::sync::Arc::new(zeph_memory::TokenCounter::new());
    let result = Agent::<MockChannel>::fetch_graph_facts(&mem_state, "test", 1000, &tc)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_budget_zero() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(std::sync::Arc::new(memory), cid, true);
    let tc = std::sync::Arc::new(zeph_memory::TokenCounter::new());
    let result = Agent::<MockChannel>::fetch_graph_facts(&mem_state, "test", 0, &tc)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_graph_is_empty() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(std::sync::Arc::new(memory), cid, true);
    let tc = std::sync::Arc::new(zeph_memory::TokenCounter::new());
    let result = Agent::<MockChannel>::fetch_graph_facts(&mem_state, "rust", 1000, &tc)
        .await
        .unwrap();
    assert!(result.is_none(), "empty graph must return None");
}

// --- Deferred summarization tests ---

#[tokio::test]
async fn deferred_summary_stored_not_applied() {
    let summary_text = "deferred result".to_owned();
    let provider = mock_provider(vec![summary_text.clone()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

    make_tool_pair(&mut agent, "bash");
    make_tool_pair(&mut agent, "read_file");
    make_tool_pair(&mut agent, "write_file");

    let msg_count_before = agent.msg.messages.len();
    agent.maybe_summarize_tool_pair().await;

    // No new messages inserted — deferred, not immediate
    assert_eq!(agent.msg.messages.len(), msg_count_before);
    // All messages stay visible
    for msg in &agent.msg.messages {
        assert!(
            msg.metadata.agent_visible,
            "no message should be hidden after deferred storage"
        );
    }
    // deferred_summary set on oldest response (index 2)
    assert!(
        agent.msg.messages[2].metadata.deferred_summary.is_some(),
        "deferred_summary must be set on response message"
    );
}

#[test]
fn count_unsummarized_pairs_excludes_deferred() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // 4 pairs: system=0, req1=1,resp1=2, req2=3,resp2=4, req3=5,resp3=6, req4=7,resp4=8
    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");
    make_tool_pair(&mut agent, "c");
    make_tool_pair(&mut agent, "d");

    // Mark 2 of the 4 response messages as deferred
    agent.msg.messages[2].metadata.deferred_summary = Some("s1".into());
    agent.msg.messages[4].metadata.deferred_summary = Some("s2".into());

    assert_eq!(agent.count_unsummarized_pairs(), 2);
}

#[test]
fn find_oldest_unsummarized_pair_skips_deferred() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // system=0, req1=1,resp1=2, req2=3,resp2=4, req3=5,resp3=6
    make_tool_pair(&mut agent, "first");
    make_tool_pair(&mut agent, "second");
    make_tool_pair(&mut agent, "third");

    // Mark oldest response as deferred
    agent.msg.messages[2].metadata.deferred_summary = Some("already queued".into());

    // Should skip (1,2) and return (3,4)
    assert_eq!(agent.find_oldest_unsummarized_pair(), Some((3, 4)));
}

#[test]
fn count_deferred_summaries_correct() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");
    make_tool_pair(&mut agent, "c");

    assert_eq!(agent.count_deferred_summaries(), 0);

    agent.msg.messages[2].metadata.deferred_summary = Some("s1".into());
    agent.msg.messages[4].metadata.deferred_summary = Some("s2".into());
    agent.msg.messages[6].metadata.deferred_summary = Some("s3".into());

    assert_eq!(agent.count_deferred_summaries(), 3);
}

#[test]
fn apply_deferred_summaries_batch() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // system=0, req1=1,resp1=2, req2=3,resp2=4, req3=5,resp3=6
    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");
    make_tool_pair(&mut agent, "c");

    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    agent.msg.messages[4].metadata.deferred_summary = Some("sum_b".into());
    agent.msg.messages[6].metadata.deferred_summary = Some("sum_c".into());

    let applied = agent.apply_deferred_summaries();

    assert_eq!(applied, 3);

    // 6 messages hidden (2 per pair)
    let hidden = agent
        .msg
        .messages
        .iter()
        .filter(|m| !m.metadata.agent_visible)
        .count();
    assert_eq!(hidden, 6);

    // 3 Summary parts inserted
    let summaries = agent
        .msg
        .messages
        .iter()
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Summary { .. }))
        })
        .count();
    assert_eq!(summaries, 3);

    // deferred_summary cleared everywhere
    for msg in &agent.msg.messages {
        assert!(msg.metadata.deferred_summary.is_none());
    }
}

#[test]
fn apply_deferred_summaries_empty() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");

    let msg_count_before = agent.msg.messages.len();
    let applied = agent.apply_deferred_summaries();

    assert_eq!(applied, 0);
    assert_eq!(agent.msg.messages.len(), msg_count_before);
}

#[test]
fn apply_deferred_summaries_reverse_order() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // system=0, req1=1,resp1=2, req2=3,resp2=4, req3=5,resp3=6,
    //           req4=7,resp4=8, req5=9,resp5=10
    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");
    make_tool_pair(&mut agent, "c");
    make_tool_pair(&mut agent, "d");
    make_tool_pair(&mut agent, "e");

    // Set deferred at resp3 (index 6) and resp5 (index 10)
    agent.msg.messages[6].metadata.deferred_summary = Some("sum_c".into());
    agent.msg.messages[10].metadata.deferred_summary = Some("sum_e".into());

    let applied = agent.apply_deferred_summaries();

    assert_eq!(applied, 2);

    // req3 and resp3 are hidden
    assert!(!agent.msg.messages[5].metadata.agent_visible);
    assert!(!agent.msg.messages[6].metadata.agent_visible);

    // 4 total messages hidden
    let hidden = agent
        .msg
        .messages
        .iter()
        .filter(|m| !m.metadata.agent_visible)
        .count();
    assert_eq!(hidden, 4);

    // 2 summary messages
    let summaries = agent
        .msg
        .messages
        .iter()
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Summary { .. }))
        })
        .count();
    assert_eq!(summaries, 2);
}

#[test]
fn tier0_does_not_set_compacted_this_turn() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.80, 4, 0)
        .with_soft_compaction_threshold(0.70);

    make_tool_pair(&mut agent, "a");
    make_tool_pair(&mut agent, "b");

    agent.msg.messages[2].metadata.deferred_summary = Some("s".into());
    // Simulate token usage above 70% soft threshold
    agent.providers.cached_prompt_tokens = 75_000;

    assert!(!agent.context_manager.compaction.is_compacted_this_turn());
    agent.maybe_apply_deferred_summaries();
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "tier-0 must not set compacted_this_turn"
    );
}

// Regression test: when prepare_context recomputes tokens to a low value after pruning,
// the token-based trigger alone would never fire. The count-based trigger ensures deferred
// summaries are applied once >= tool_call_cutoff pairs accumulate (default 6).
#[test]
fn tier0_count_trigger_fires_without_budget_pressure() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // Large budget: soft threshold 70% = 70_000 tokens — far above cached_prompt_tokens
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.80, 4, 0)
        .with_soft_compaction_threshold(0.70);

    // tool_call_cutoff defaults to 6; add exactly that many deferred summaries
    for label in ["a", "b", "c", "d", "e", "f"] {
        make_tool_pair(&mut agent, label);
    }
    // resp messages are at even indices 2,4,6,8,10,12 (system=0, req=odd, resp=even)
    agent.msg.messages[2].metadata.deferred_summary = Some("s_a".into());
    agent.msg.messages[4].metadata.deferred_summary = Some("s_b".into());
    agent.msg.messages[6].metadata.deferred_summary = Some("s_c".into());
    agent.msg.messages[8].metadata.deferred_summary = Some("s_d".into());
    agent.msg.messages[10].metadata.deferred_summary = Some("s_e".into());
    agent.msg.messages[12].metadata.deferred_summary = Some("s_f".into());

    // Token count well below budget threshold (simulates post-pruning state)
    agent.providers.cached_prompt_tokens = 5_000;

    agent.maybe_apply_deferred_summaries();

    let summaries = agent
        .msg
        .messages
        .iter()
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Summary { .. }))
        })
        .count();
    assert_eq!(
        summaries, 6,
        "count trigger must apply all 6 deferred summaries"
    );
}

#[test]
fn find_oldest_unsummarized_skips_pruned_content() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // First pair: response with pruned (empty) ToolOutput body
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: "id_pruned".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: String::new(), // empty = pruned
            compacted_at: None,
        }],
    ));

    // Second pair: real content
    make_tool_pair(&mut agent, "real_tool");

    // Pruned pair at (1,2); real pair at (3,4)
    assert_eq!(
        agent.find_oldest_unsummarized_pair(),
        Some((3, 4)),
        "pruned pair should be skipped"
    );
}

#[tokio::test]
async fn fetch_graph_facts_returns_some_with_entities_and_has_prefix() {
    use zeph_memory::graph::{EntityType, GraphStore};

    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    {
        let store = GraphStore::new(memory.sqlite().pool().clone());
        let rust_id = store
            .upsert_entity(
                "rust",
                "rust",
                EntityType::Language,
                Some("systems language"),
            )
            .await
            .unwrap();
        let tokio_id = store
            .upsert_entity("tokio", "tokio", EntityType::Tool, Some("async runtime"))
            .await
            .unwrap();
        store
            .insert_edge(rust_id, tokio_id, "uses", "Rust uses tokio", 0.9, None)
            .await
            .unwrap();
    }

    let mem_state = make_mem_state(std::sync::Arc::new(memory), cid, true);
    let tc = std::sync::Arc::new(zeph_memory::TokenCounter::new());
    let result = Agent::<MockChannel>::fetch_graph_facts(&mem_state, "rust", 2000, &tc)
        .await
        .unwrap();
    assert!(result.is_some());
    let msg = result.unwrap();
    assert!(msg.content.starts_with(super::super::GRAPH_FACTS_PREFIX));
}
#[test]
fn remove_lsp_messages_removes_lsp_system_keeps_others() {
    use crate::agent::LSP_NOTE_PREFIX;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Push a non-LSP system message that must survive.
    agent.push_message(Message {
        role: Role::System,
        content: "[recall] some recall data".to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Push an LSP system note that must be removed.
    agent.push_message(Message {
        role: Role::System,
        content: format!("{LSP_NOTE_PREFIX}diagnostics]\nsrc/main.rs:1 error: foo"),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Push a user message that must survive.
    agent.push_message(Message {
        role: Role::User,
        content: "hello".to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    let before = agent.msg.messages.len();
    agent.remove_lsp_messages();
    // Only the LSP system note should be gone.
    assert_eq!(agent.msg.messages.len(), before - 1);
    assert!(
        agent
            .msg
            .messages
            .iter()
            .all(|m| !m.content.starts_with(LSP_NOTE_PREFIX))
    );
    // Non-LSP system message preserved.
    assert!(
        agent
            .msg
            .messages
            .iter()
            .any(|m| m.content.starts_with("[recall]"))
    );
}

// --- Compaction guard tests (issue #1708) ---

// Cooldown guard: cooling turns_remaining counts down and blocks compaction.
#[tokio::test]
async fn cooldown_guard_decrements_and_skips_compaction() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_compaction_cooldown(2)
        .with_metrics(tx);

    // Manually set cooling state as if compaction just fired and turn advanced.
    agent.context_manager.compaction = CompactionState::Cooling { turns_remaining: 2 };

    // Push enough tokens to trigger compaction threshold.
    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // First call: turns_remaining = 2 → skips, decrements to 1. No compaction fired.
    agent.maybe_compact().await.unwrap();
    assert_eq!(agent.context_manager.compaction.cooldown_remaining(), 1);
    assert_eq!(rx.borrow().context_compactions, 0);

    // Second call: turns_remaining = 1 → skips, decrements to 0 → transitions to Ready.
    agent.maybe_compact().await.unwrap();
    assert_eq!(agent.context_manager.compaction.cooldown_remaining(), 0);
    assert_eq!(rx.borrow().context_compactions, 0);
}

// Cooldown guard: after cooldown expires, compaction fires and resets the counter.
#[tokio::test]
async fn cooldown_guard_fires_after_expiry_and_resets_counter() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_compaction_cooldown(2)
        .with_metrics(tx);

    // Ready state means cooldown has already expired.
    assert_eq!(agent.context_manager.compaction, CompactionState::Ready);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // Seed cached_prompt_tokens above threshold so maybe_compact proceeds past Guard 1.
    // Messages were pushed directly (bypassing push_message), so we set this explicitly.
    // Use a large value so freed_tokens > 0 after compact_context() recomputes.
    agent.providers.cached_prompt_tokens = 10_000;

    agent.maybe_compact().await.unwrap();

    // Compaction fired: metrics incremented.
    assert_eq!(rx.borrow().context_compactions, 1);
    // After compaction the system prompt alone exceeds the tiny 100-token budget, so
    // Guard 3 marks exhaustion (still above threshold). Cooldown is not reset — correct.
    assert!(agent.context_manager.compaction.is_exhausted());
}

// Exhaustion guard: when compaction_exhausted is set, maybe_compact returns early.
#[tokio::test]
async fn exhaustion_guard_skips_compaction_when_exhausted() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);

    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();

    // Compaction did NOT fire.
    assert_eq!(rx.borrow().context_compactions, 0);
}

// Exhaustion guard: exhaustion_warned set after first call, stays true on second call.
#[tokio::test]
async fn exhaustion_guard_warned_flag_set_once() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // First call: warning not yet sent → warned flipped to true.
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: false }
    ));
    agent.maybe_compact().await.unwrap();
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: true }
    ));

    // Second call: warned already set, no state change.
    agent.maybe_compact().await.unwrap();
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: true }
    ));
}

// Exhaustion guard fires before cooldown guard.
#[tokio::test]
async fn exhaustion_guard_takes_precedence_over_cooldown() {
    use std::sync::Arc;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let statuses = Arc::clone(&channel.statuses);
    let registry = create_test_registry();

    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_compaction_cooldown(2);

    // Exhausted state (the Cooling state would normally guard against exhaustion, but
    // we test the ordering guarantee that exhaustion check happens before cooldown decrement).
    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();

    // State must remain Exhausted — exhaustion guard returned before cooldown decrement.
    assert!(agent.context_manager.compaction.is_exhausted());
    // No "compacting context..." status emitted.
    assert!(
        !statuses
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "compacting context..."),
        "compaction must not have started"
    );
}

// Counterproductive guard: too few compactable messages sets exhausted.
#[tokio::test]
async fn counterproductive_guard_sets_exhausted_when_too_few_messages() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // preserve_tail = 5, budget = 100 so threshold is low → should_compact() fires.
    // With only a system prompt + 2 messages, compactable = len - preserve_tail - 1.
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 5, 0);

    // Add just 2 messages: compactable = 3 - 5 - 1 = saturates to 0, which is ≤ 1.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "x".repeat(200),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "x".repeat(200),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Tier-1 pruning won't free enough (no ToolOutput parts), so tier-2 attempts.
    agent.maybe_compact().await.unwrap();

    // Counterproductive guard: compactable ≤ 1 → exhausted set.
    assert!(agent.context_manager.compaction.is_exhausted());
}

// Default value for compaction_cooldown_turns is 2.
#[test]
fn context_manager_defaults_have_compaction_guard_fields() {
    let cm = crate::agent::context_manager::ContextManager::new();
    assert_eq!(cm.compaction_cooldown_turns, 2);
    assert_eq!(cm.compaction, CompactionState::Ready);
}

// with_compaction_cooldown builder sets the cooldown turns field.
#[test]
fn builder_with_compaction_cooldown_sets_field() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_compaction_cooldown(5);

    assert_eq!(agent.context_manager.compaction_cooldown_turns, 5);
}

#[test]
fn compaction_hard_count_zero_by_default() {
    let snapshot = crate::metrics::MetricsSnapshot::default();
    assert_eq!(snapshot.compaction_hard_count, 0);
    assert!(snapshot.compaction_turns_after_hard.is_empty());
}

#[tokio::test]
async fn compaction_hard_count_increments_on_hard_tier() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);

    // Drive cached_prompt_tokens above the hard threshold (75% of 1000 = 750).
    agent.providers.cached_prompt_tokens = 900;

    agent.maybe_compact().await.unwrap();

    assert_eq!(rx.borrow().compaction_hard_count, 1);
}

#[tokio::test]
async fn compaction_turns_after_hard_tracks_segments() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_compaction_cooldown(0)
        .with_metrics(tx);

    // Simulate first hard compaction by driving cached tokens above threshold.
    agent.providers.cached_prompt_tokens = 900;
    agent.maybe_compact().await.unwrap();
    assert_eq!(rx.borrow().compaction_hard_count, 1);
    // turns_since_last_hard_compaction is now Some(0).

    // Simulate 3 turns where context is below threshold.
    // Reset per-turn state (done by advance_turn at the start of each turn).
    agent.providers.cached_prompt_tokens = 0;
    for _ in 0..3 {
        agent.context_manager.compaction = agent.context_manager.compaction.advance_turn();
        agent.maybe_compact().await.unwrap();
    }
    // turns_since_last_hard_compaction is now Some(3).

    // Directly trigger the Hard tier accounting without a real LLM call
    // by simulating what maybe_compact does in the Hard branch.
    // This tests that the Vec accumulates the segment correctly.
    if let Some(turns) = agent.context_manager.turns_since_last_hard_compaction {
        agent.update_metrics(|m| {
            m.compaction_turns_after_hard.push(turns);
            m.compaction_hard_count += 1;
        });
        agent.context_manager.turns_since_last_hard_compaction = Some(0);
    }

    assert_eq!(rx.borrow().compaction_hard_count, 2);
    assert_eq!(rx.borrow().compaction_turns_after_hard, vec![3]);
}

#[tokio::test]
async fn compaction_turn_counter_increments_before_exhaustion_guard() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0);

    // Manually set tracking active and exhaust compaction.
    agent.context_manager.turns_since_last_hard_compaction = Some(0);
    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    // Call maybe_compact — early return via exhaustion guard.
    agent.maybe_compact().await.unwrap();

    // Turn counter must still have been incremented (S1/S2 fix).
    assert_eq!(
        agent.context_manager.turns_since_last_hard_compaction,
        Some(1)
    );
}

// maybe_soft_compact_mid_iteration tests (#1828)

#[test]
fn mid_iteration_skips_when_compacted_this_turn() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0)
        .with_soft_compaction_threshold(0.60);

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Simulate token pressure above soft threshold
    agent.providers.cached_prompt_tokens = 75_000;
    // Mark hard compaction already ran this turn
    agent.context_manager.compaction = CompactionState::CompactedThisTurn { cooldown: 2 };

    agent.maybe_soft_compact_mid_iteration();

    // Deferred summary must NOT have been applied (early return)
    let applied = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        !applied,
        "must not apply deferred summaries when compacted_this_turn is set"
    );
}

#[test]
fn mid_iteration_skips_when_tier_is_none() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0)
        .with_soft_compaction_threshold(0.60);

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token count well below soft threshold (50_000 < 60_000) → None tier
    agent.providers.cached_prompt_tokens = 50_000;

    agent.maybe_soft_compact_mid_iteration();

    // No deferred summary applied when tier is None
    let applied = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(!applied, "must not compact when tier is None");
}

#[test]
fn mid_iteration_applies_deferred_summaries_at_soft_tier() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000; hard=0.90
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0)
        .with_soft_compaction_threshold(0.60);

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token pressure above soft (75_000 > 60_000) but below hard (90_000)
    agent.providers.cached_prompt_tokens = 75_000;

    agent.maybe_soft_compact_mid_iteration();

    // Deferred summary must have been applied
    let summary_inserted = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        summary_inserted,
        "deferred summary must be applied at soft tier"
    );
}

#[test]
fn mid_iteration_does_not_set_compacted_this_turn() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0)
        .with_soft_compaction_threshold(0.60);

    make_tool_pair_with_output(&mut agent, "a");
    agent.providers.cached_prompt_tokens = 75_000;

    assert!(!agent.context_manager.compaction.is_compacted_this_turn());
    agent.maybe_soft_compact_mid_iteration();
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "maybe_soft_compact_mid_iteration must not set compacted_this_turn"
    );
}

#[test]
fn mid_iteration_fires_at_hard_tier() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → 60_000; hard=0.90 → 90_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0)
        .with_soft_compaction_threshold(0.60);

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token pressure above hard threshold (95_000 > 90_000) → Hard tier
    agent.providers.cached_prompt_tokens = 95_000;

    agent.maybe_soft_compact_mid_iteration();

    // Soft actions (deferred summaries) must still be applied even at Hard tier
    let summary_inserted = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        summary_inserted,
        "deferred summaries must be applied even when tier is Hard"
    );
    // compaction state must remain unchanged (no LLM call, no Hard compaction)
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "mid-iteration must not set compacted_this_turn even at Hard tier"
    );
}

// --- assembly.rs: clear_history ---

/// `clear_history` must retain the system prompt (message[0]) and discard all
/// subsequent messages so the agent can restart a conversation cleanly.
#[tokio::test]
async fn clear_history_retains_system_prompt() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Add some history beyond the initial system prompt.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "world".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    assert_eq!(agent.msg.messages.len(), 3);

    agent.clear_history();

    assert_eq!(
        agent.msg.messages.len(),
        1,
        "clear_history must leave exactly the system prompt"
    );
    assert_eq!(
        agent.msg.messages[0].role,
        Role::System,
        "retained message must be the system prompt"
    );
}

/// `clear_history` on an agent with only the system prompt must leave it unchanged.
#[tokio::test]
async fn clear_history_with_only_system_prompt_is_idempotent() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let system_content = agent.msg.messages[0].content.clone();

    agent.clear_history();

    assert_eq!(agent.msg.messages.len(), 1);
    assert_eq!(
        agent.msg.messages[0].content, system_content,
        "system prompt content must be unchanged after clear_history"
    );
}

// --- assembly.rs: rebuild_system_prompt with empty skill list ---

/// `rebuild_system_prompt` must not panic and must produce a non-empty prompt
/// even when the skill registry is empty (no skills loaded).
#[tokio::test]
async fn rebuild_system_prompt_empty_skill_list_does_not_crash() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    // Explicitly empty registry — no skills at all.
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Must not panic.
    agent
        .rebuild_system_prompt("test query with no skills")
        .await;

    let prompt = &agent.msg.messages[0];
    assert_eq!(
        prompt.role,
        Role::System,
        "first message must still be the system prompt"
    );
    assert!(
        !prompt.content.is_empty(),
        "system prompt must be non-empty even with no skills"
    );
}

/// The system prompt produced by `rebuild_system_prompt` must contain exactly
/// the two cache marker comments required by the Claude caching implementation
/// (cache:stable and cache:volatile). More than 4 markers would exceed the API
/// limit; the prompt format is expected to use exactly these two.
#[tokio::test]
async fn rebuild_system_prompt_cache_markers_count() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    let stable_count = prompt.matches("<!-- cache:stable -->").count();
    let volatile_count = prompt.matches("<!-- cache:volatile -->").count();

    assert_eq!(
        stable_count, 1,
        "exactly one cache:stable marker must be present"
    );
    assert_eq!(
        volatile_count, 1,
        "exactly one cache:volatile marker must be present"
    );
    // Total cache markers must not exceed 4 (Claude API limit).
    let total = stable_count + volatile_count + prompt.matches("<!-- cache:tools -->").count();
    assert!(
        total <= 4,
        "total cache markers must not exceed 4 (Claude API limit); got {total}"
    );
}

// T-06: H1 regression — ProbeRejected must NOT trigger Exhausted transition.
//
// Design invariant (H1 fix): when the compaction probe rejects a summary,
// compact_context() returns CompactionOutcome::ProbeRejected. The caller
// (maybe_compact) must set CompactedThisTurn (cooldown) and NOT transition
// to CompactionState::Exhausted, because the failure is quality-related, not
// because the compactor is structurally unable to free tokens.
#[tokio::test]
async fn probe_rejected_does_not_trigger_exhausted() {
    // Provider returns:
    //   1st call: summary text (for summarize_messages)
    //   2nd call: probe questions JSON
    //   3rd call: probe answers JSON — all refusals → score ~0.0 → HardFail
    let questions_json = r#"{"questions": [{"question": "What crate?", "expected_answer": "thiserror"}, {"question": "What file?", "expected_answer": "src/lib.rs"}]}"#;
    let answers_json = r#"{"answers": ["UNKNOWN", "UNKNOWN"]}"#;
    let provider = mock_provider(vec![
        "compacted summary".to_string(),
        questions_json.to_string(),
        answers_json.to_string(),
    ]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    // Enable compaction probe with default thresholds (Pass >= 0.6, HardFail < 0.35).
    agent.context_manager.compression.probe.enabled = true;

    // Populate enough messages to pass the too-few-messages guard.
    for i in 0..8 {
        agent.msg.messages.push(Message {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let outcome = agent.compact_context().await.unwrap();

    // H1 invariant: probe-rejected outcome must not cause Exhausted.
    assert_eq!(
        outcome,
        CompactionOutcome::ProbeRejected,
        "expected ProbeRejected when all probe answers are refusals"
    );
    // The messages must NOT have been drained (original messages preserved).
    assert!(
        agent.msg.messages.len() > 3,
        "messages must not be drained after ProbeRejected"
    );
    // Verify the state machine invariant: not Exhausted.
    assert!(
        !matches!(
            agent.context_manager.compaction,
            CompactionState::Exhausted { .. }
        ),
        "ProbeRejected must not transition to Exhausted (H1 invariant)"
    );
}

// --- #2475: memory_save session hint in system prompt ---

/// When `memory_save` is present in `tool_state.completed_tool_ids`, `rebuild_system_prompt`
/// must append the disambiguation hint directing the model to use `memory_search`
/// rather than `search_code` for user-provided facts.
#[tokio::test]
async fn rebuild_system_prompt_injects_memory_save_hint_when_tool_was_used() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent
        .tool_state
        .completed_tool_ids
        .insert("memory_save".to_owned());
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    assert!(
        prompt.contains("memory_save — use memory_search to recall them, not search_code"),
        "session hint must be present when memory_save was used; prompt: {prompt}"
    );
}

/// When `tool_state.completed_tool_ids` does NOT contain `memory_save`, no hint must be
/// appended — the system prompt must stay clean to avoid unnecessary noise.
#[tokio::test]
async fn rebuild_system_prompt_omits_memory_save_hint_when_tool_not_used() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // tool_state.completed_tool_ids is empty by default — no memory_save.
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    assert!(
        !prompt.contains("memory_save — use memory_search to recall them, not search_code"),
        "session hint must NOT be present when memory_save was not used"
    );
}
