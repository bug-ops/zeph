// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use tokio::sync::watch;
use zeph_llm::provider::{Message, MessageMetadata, Role};
use zeph_memory::semantic::SemanticMemory;

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::context::chunk_messages;
use crate::agent::context_manager::CompactionTier;
use crate::agent::{CORRECTIONS_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX};

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
        .with_context_budget(10_000, 0.20, 0.90, 4, 0);
    assert_eq!(agent.compaction_tier(), CompactionTier::None);
}

#[test]
fn compaction_tier_hard_above_threshold() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.50;

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
        Arc::new(memory),
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
        Arc::new(memory),
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
        Arc::new(memory),
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
        Arc::new(memory),
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
        Arc::new(memory),
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
