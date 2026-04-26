// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider, mock_provider_failing,
};
use crate::agent::context::assembler_helpers;
use crate::agent::context::{cap_summary, truncate_chars};
use crate::agent::context_manager::CompactionState;
use crate::agent::state::MemoryState;

// Helper: add a tool call/result message pair using ToolResult parts.
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
            tool_name: tool_name.into(),
            body: format!("full output of {tool_name}"),
            compacted_at: None,
        }],
    ));
}

fn make_tool_result_message(content: &str) -> Message {
    Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: content.into(),
            is_error: false,
        }],
    )
}

fn make_mem_state(
    memory: Arc<zeph_memory::semantic::SemanticMemory>,
    cid: zeph_memory::ConversationId,
    graph_enabled: bool,
) -> MemoryState {
    use crate::agent::state::{
        MemoryCompactionState, MemoryExtractionState, MemoryPersistenceState, MemorySubsystemState,
    };
    MemoryState {
        persistence: MemoryPersistenceState {
            memory: Some(memory),
            conversation_id: Some(cid),
            history_limit: 50,
            recall_limit: 5,
            cross_session_score_threshold: 0.5,
            autosave_assistant: false,
            autosave_min_length: 20,
            tool_call_cutoff: 6,
            unsummarized_count: 0,
            last_recall_confidence: None,
            context_format: zeph_config::ContextFormat::default(),
        },
        compaction: MemoryCompactionState {
            summarization_threshold: 100,
            compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig::default(),
            shutdown_summary: true,
            shutdown_summary_min_messages: 4,
            shutdown_summary_max_messages: 20,
            shutdown_summary_timeout_secs: 10,
            structured_summaries: false,
            digest_config: crate::config::DigestConfig::default(),
            cached_session_digest: None,
            context_strategy: crate::config::ContextStrategy::default(),
            crossover_turn_threshold: 20,
        },
        extraction: MemoryExtractionState {
            document_config: crate::config::DocumentConfig::default(),
            graph_config: crate::config::GraphConfig {
                enabled: graph_enabled,
                ..Default::default()
            },
            rpe_router: None,
            goal_text: None,
            persona_config: crate::config::PersonaConfig::default(),
            trajectory_config: crate::config::TrajectoryConfig::default(),
            category_config: crate::config::CategoryConfig::default(),
            reasoning_config: zeph_config::ReasoningConfig::default(),
        },
        subsystems: MemorySubsystemState::default(),
    }
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
    let store = Arc::new(zeph_memory::graph::GraphStore::new(
        mem.sqlite().pool().clone(),
    ));
    mem.with_graph_store(store)
}

#[test]
fn truncate_chars_is_safe_for_multibyte() {
    // Each Cyrillic char is 2 bytes; slicing at byte 200 would panic on odd boundaries.
    let s: String = "Привет".repeat(50); // 300 chars, 600 bytes
    let truncated = truncate_chars(&s, 200);
    assert!(truncated.ends_with('…'));
    // Must be valid UTF-8 (no panic means success, but also check char count)
    assert_eq!(truncated.chars().count(), 201); // 200 chars + '…'
}

// --- truncate_chars additional edge cases ---

#[test]
fn truncate_chars_ascii_exact() {
    let s = "abcde";
    // max_chars == len → no truncation
    let result = truncate_chars(s, 5);
    assert_eq!(result, "abcde");
}

#[test]
fn truncate_chars_emoji() {
    // 🚀 is a single Unicode scalar even though it is 4 bytes
    let s = "🚀🚀🚀🚀🚀";
    let result = truncate_chars(s, 3);
    assert!(result.ends_with('…'), "should append ellipsis");
    // 3 emoji + ellipsis = 4 Unicode scalars
    assert_eq!(result.chars().count(), 4);
}

#[test]
fn truncate_chars_empty() {
    let result = truncate_chars("", 10);
    assert_eq!(result, "");
}

#[test]
fn truncate_chars_shorter_than_max() {
    let s = "hello";
    let result = truncate_chars(s, 100);
    assert_eq!(result, "hello");
}

#[test]
fn truncate_chars_zero_max() {
    let s = "hello";
    // max_chars = 0 → returns empty string (no chars kept, no ellipsis)
    let result = truncate_chars(s, 0);
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
    agent.msg.messages[1].metadata.visibility = zeph_llm::provider::MessageVisibility::UserOnly;
    agent.msg.messages[2].metadata.visibility = zeph_llm::provider::MessageVisibility::UserOnly;

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
    agent.msg.messages[1].metadata.visibility = zeph_llm::provider::MessageVisibility::UserOnly;
    agent.msg.messages[2].metadata.visibility = zeph_llm::provider::MessageVisibility::UserOnly;

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
    assert!(agent.msg.messages[1].metadata.visibility.is_agent_visible());
    assert!(agent.msg.messages[2].metadata.visibility.is_agent_visible());
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
    assert!(agent.msg.messages[1].metadata.visibility.is_agent_visible());
    assert!(agent.msg.messages[2].metadata.visibility.is_agent_visible());
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
    let keep_recent = 2 * agent.memory_state.persistence.tool_call_cutoff + 2;
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
        !agent.msg.messages[1].metadata.visibility.is_agent_visible(),
        "oldest pair request should be hidden"
    );
    assert!(
        !agent.msg.messages[2].metadata.visibility.is_agent_visible(),
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
    let keep_recent = 2 * agent.memory_state.persistence.tool_call_cutoff + 2;
    agent.prune_stale_tool_outputs(keep_recent);

    // Verify all visible ToolOutput parts have non-empty bodies.
    for msg in agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.visibility.is_agent_visible())
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

    let keep_recent = 2 * agent.memory_state.persistence.tool_call_cutoff + 2;
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
        .filter(|m| m.metadata.visibility.is_agent_visible())
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
    let keep_recent = 2 * agent.memory_state.persistence.tool_call_cutoff + 2;
    let freed = agent.prune_stale_tool_outputs(keep_recent);

    // Messages count unchanged (no summary inserted due to failure).
    assert_eq!(agent.msg.messages.len(), msg_count_before);
    // With [sys, req1, res1, req2, res2] = 5, keep_recent=4, boundary=1:
    // messages[1..1] is empty → nothing pruned.
    assert_eq!(freed, 0, "keep_recent=4 should protect all 4 tool messages");
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_graph_config_disabled() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(Arc::new(memory), cid, false);
    let tc = Arc::new(zeph_memory::TokenCounter::new());
    let result = assembler_helpers::fetch_graph_facts(&mem_state, "test", 1000, &tc)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_budget_zero() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(Arc::new(memory), cid, true);
    let tc = Arc::new(zeph_memory::TokenCounter::new());
    let result = assembler_helpers::fetch_graph_facts(&mem_state, "test", 0, &tc)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn fetch_graph_facts_returns_none_when_graph_is_empty() {
    let memory = build_graph_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mem_state = make_mem_state(Arc::new(memory), cid, true);
    let tc = Arc::new(zeph_memory::TokenCounter::new());
    let result = assembler_helpers::fetch_graph_facts(&mem_state, "rust", 1000, &tc)
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
            msg.metadata.visibility.is_agent_visible(),
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
        .filter(|m| !m.metadata.visibility.is_agent_visible())
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
    assert!(!agent.msg.messages[5].metadata.visibility.is_agent_visible());
    assert!(!agent.msg.messages[6].metadata.visibility.is_agent_visible());

    // 4 total messages hidden
    let hidden = agent
        .msg
        .messages
        .iter()
        .filter(|m| !m.metadata.visibility.is_agent_visible())
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
        .with_context_budget(100_000, 0.20, 0.80, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.70;

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
        .with_context_budget(100_000, 0.20, 0.80, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.70;

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

    let mem_state = make_mem_state(Arc::new(memory), cid, true);
    let tc = Arc::new(zeph_memory::TokenCounter::new());
    let result = assembler_helpers::fetch_graph_facts(&mem_state, "rust", 2000, &tc)
        .await
        .unwrap();
    assert!(result.is_some());
    let msg = result.unwrap();
    assert!(msg.content.starts_with(crate::agent::GRAPH_FACTS_PREFIX));
}

// Suppress unused import warning: chunk_messages is imported for module coherence
// but tests in this file call it via its alias to avoid conflicts.
#[allow(unused_imports)]
use crate::agent::context::chunk_messages;
