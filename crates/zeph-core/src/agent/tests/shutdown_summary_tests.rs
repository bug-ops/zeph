// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{Message, MessageMetadata, Role};
use zeph_memory::semantic::SemanticMemory;

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

#[tokio::test]
async fn shutdown_summary_disabled_skips_llm() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_shutdown_summary_config(false, 4, 20, 10);

    // Add enough user messages to exceed the threshold.
    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("user message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_store_shutdown_summary().await;

    // LLM must not be called when feature is disabled.
    assert!(
        recorded.lock().unwrap().is_empty(),
        "LLM must not be called when shutdown_summary is disabled"
    );
}

#[tokio::test]
async fn shutdown_summary_no_memory_skips_llm() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // No .with_memory() call — memory_state.persistence.memory is None.
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_shutdown_summary_config(true, 4, 20, 10);

    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("user message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_store_shutdown_summary().await;

    assert!(
        recorded.lock().unwrap().is_empty(),
        "LLM must not be called when no memory backend is attached"
    );
}

#[tokio::test]
async fn shutdown_summary_too_few_user_messages_skips_llm() {
    use std::sync::Arc;

    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock.clone());
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        AnyProvider::Mock(MockProvider::default()),
        "test-model",
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // min_messages=4 but we will only add 2 user messages.
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(Arc::new(memory), cid, 100, 5, 1000)
        .with_shutdown_summary_config(true, 4, 20, 10);

    // System prompt is messages[0] — skip(1) counts from index 1.
    // Add 2 user messages: below the threshold of 4.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "first user message".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "assistant reply".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "second user message".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.maybe_store_shutdown_summary().await;

    assert!(
        recorded.lock().unwrap().is_empty(),
        "LLM must not be called when user message count is below min_messages"
    );
}

#[tokio::test]
async fn shutdown_summary_only_counts_user_role_messages() {
    use std::sync::Arc;

    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        AnyProvider::Mock(MockProvider::default()),
        "test-model",
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // min_messages=4: need at least 4 user messages.
    // We add 8 assistant messages but only 3 user messages — should still skip.
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(Arc::new(memory), cid, 100, 5, 1000)
        .with_shutdown_summary_config(true, 4, 20, 10);

    for _ in 0..8 {
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "assistant reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }
    for i in 0..3 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("user message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_store_shutdown_summary().await;

    assert!(
        recorded.lock().unwrap().is_empty(),
        "assistant messages must not count toward min_messages threshold"
    );
}

#[tokio::test]
async fn with_shutdown_summary_config_builder_sets_fields() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_shutdown_summary_config(false, 7, 15, 10);

    assert!(!agent.services.memory.compaction.shutdown_summary);
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_min_messages,
        7
    );
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_max_messages,
        15
    );
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_timeout_secs,
        10
    );
}

#[tokio::test]
async fn shutdown_summary_default_config_values() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    assert!(
        agent.services.memory.compaction.shutdown_summary,
        "shutdown_summary must be enabled by default"
    );
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_min_messages,
        4,
        "default min_messages must be 4"
    );
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_max_messages,
        20,
        "default max_messages must be 20"
    );
    assert_eq!(
        agent
            .services
            .memory
            .compaction
            .shutdown_summary_timeout_secs,
        30,
        "default timeout_secs must be 30"
    );
}

// --- Doom-loop integration tests ---

/// The real doom-loop detection lives in the agent's native tool loop. This test
/// verifies that when the `MockProvider` (with `tool_use=true`) returns identical tool
/// outputs `DOOM_LOOP_WINDOW` times in a row, the agent breaks the loop and sends
/// the expected stopping message instead of running forever.
///
/// Each iteration uses different tool input args to bypass the repeat-detection
/// mechanism (which operates on `args_hash`), ensuring only the doom-loop detector
/// (which operates on output content) is exercised.
#[tokio::test]
async fn doom_loop_agent_breaks_on_identical_native_tool_outputs() {
    use crate::agent::DOOM_LOOP_WINDOW;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};

    // Each ChatResponse has a unique id and different input args (to avoid
    // repeat-detection which fires on identical (name, args_hash) pairs),
    // but the tool executor always returns Ok(None) → "(no output)" each time.
    // After DOOM_LOOP_WINDOW identical last-message contents, doom-loop fires.
    let tool_responses: Vec<ChatResponse> = (0..=DOOM_LOOP_WINDOW)
        .map(|i| ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: format!("toolu_{i:06}"),
                name: "stub_tool".to_owned().into(),
                // Vary the input so args_hash differs each iteration → no repeat-detect
                input: serde_json::json!({ "iteration": i }),
            }],
            thinking_blocks: vec![],
        })
        .collect();

    let (mock, _counter) = MockProvider::default().with_tool_use(tool_responses);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["trigger doom loop".to_owned()]);
    let registry = create_test_registry();
    // Default MockToolExecutor::execute_tool_call returns Ok(None) → "(no output)"
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;

    assert!(
        result.is_ok(),
        "agent must not return an error on doom loop"
    );

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter()
            .any(|m| m.contains("Stopping: detected repeated identical tool outputs.")),
        "agent must send the doom-loop stopping message; got: {sent:?}"
    );
}

// Tests for filter_stats metric propagation (issue #1939).
// The normal native tool path (single tool call) must increment filter_* metrics when the
// tool returns FilterStats.

#[tokio::test]
async fn filter_stats_metrics_increment_on_normal_native_tool_path() {
    use crate::metrics::MetricsSnapshot;
    use tokio::sync::watch;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    use zeph_tools::executor::{FilterStats, ToolCall, ToolError, ToolExecutor, ToolOutput};

    struct FilteredToolExecutor;

    impl ToolExecutor for FilteredToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: "shell".into(),
                summary: "filtered output".to_owned(),
                blocks_executed: 1,
                filter_stats: Some(FilterStats {
                    raw_chars: 400,
                    filtered_chars: 200,
                    raw_lines: 20,
                    filtered_lines: 10,
                    confidence: None,
                    command: None,
                    kept_lines: vec![],
                }),
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }

    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: "call-1".to_owned(),
                name: "shell".to_owned().into(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".to_owned()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["run a tool".to_owned()]);
    let registry = create_test_registry();
    let executor = FilteredToolExecutor;
    let (tx, rx) = watch::channel(MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
    agent.run().await.expect("agent run must succeed");

    let snap: MetricsSnapshot = rx.borrow().clone();
    assert!(
        snap.filter_applications > 0,
        "filter_applications must be > 0"
    );
    assert!(snap.filter_raw_tokens > 0, "filter_raw_tokens must be > 0");
    assert!(
        snap.filter_saved_tokens > 0,
        "filter_saved_tokens must be > 0"
    );
    assert_eq!(snap.filter_total_commands, 1);
    assert_eq!(snap.filter_filtered_commands, 1);
}

// Helper: executor used by filter_stats_metrics_recorded_in_self_reflection_remaining_tools_loop.
// tool_a returns [error] (triggers self-reflection), tool_b returns FilterStats (success).
struct TwoToolExecutor {
    call_count: std::sync::Mutex<u32>,
}

impl zeph_tools::executor::ToolExecutor for TwoToolExecutor {
    async fn execute(
        &self,
        _response: &str,
    ) -> Result<Option<zeph_tools::executor::ToolOutput>, zeph_tools::executor::ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(
        &self,
        call: &zeph_tools::executor::ToolCall,
    ) -> Result<Option<zeph_tools::executor::ToolOutput>, zeph_tools::executor::ToolError> {
        let n = {
            let mut g = self.call_count.lock().unwrap();
            *g += 1;
            *g
        };
        if n == 1 || call.tool_id == "tool_a_id" {
            Ok(Some(zeph_tools::executor::ToolOutput {
                tool_name: "tool_a".into(),
                summary: "[error] command failed [exit code 1]".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        } else {
            Ok(Some(zeph_tools::executor::ToolOutput {
                tool_name: "tool_b".into(),
                summary: "filtered output".to_owned(),
                blocks_executed: 1,
                filter_stats: Some(zeph_tools::executor::FilterStats {
                    raw_chars: 400,
                    filtered_chars: 200,
                    raw_lines: 20,
                    filtered_lines: 10,
                    confidence: None,
                    command: None,
                    kept_lines: vec![],
                }),
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }
}

// Self-reflection remaining-tools path: when the first of two parallel tool calls returns
// [error] and self-reflection fires (Ok(true)), the second call's FilterStats must still
// be recorded in filter_* metrics (regression for #1939).
//
// Setup: two concurrent tool calls via native path (ToolUse response).
// tool_a returns [error], triggering self-reflection which calls chat() → Text.
// tool_b returns success with FilterStats. The remaining-tools loop processes tool_b.
#[tokio::test]
async fn filter_stats_metrics_recorded_in_self_reflection_remaining_tools_loop() {
    use crate::config::LearningConfig;
    use crate::metrics::MetricsSnapshot;
    use tokio::sync::watch;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};

    // Provider: one ToolUse response (two parallel tools), then Text for self-reflection.
    // When chat_with_tools queue is exhausted, fallback to chat() which returns "ok".
    let (mock, _counter) = MockProvider::with_responses(vec!["reflection ok".to_owned()])
        .with_tool_use(vec![ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![
                ToolUseRequest {
                    id: "tool_a_id".to_owned(),
                    name: "tool_a".to_owned().into(),
                    input: serde_json::json!({}),
                },
                ToolUseRequest {
                    id: "tool_b_id".to_owned(),
                    name: "tool_b".to_owned().into(),
                    input: serde_json::json!({}),
                },
            ],
            thinking_blocks: vec![],
        }]);

    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["run tools".to_owned()]);
    let registry = create_test_registry();
    let executor = TwoToolExecutor {
        call_count: std::sync::Mutex::new(0),
    };
    let (tx, rx) = watch::channel(MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(tx)
        .with_learning(LearningConfig {
            enabled: true,
            ..LearningConfig::default()
        });
    // Activate the "test-skill" created by create_test_registry() so self-reflection fires.
    agent
        .services
        .skill
        .active_skill_names
        .push("test-skill".to_owned());
    agent.run().await.expect("agent run must succeed");

    let snap: MetricsSnapshot = rx.borrow().clone();
    assert!(
        snap.filter_applications > 0,
        "filter_applications must be > 0 after remaining-tools loop processes tool_b"
    );
    assert!(
        snap.filter_raw_tokens > 0,
        "filter_raw_tokens must be > 0 after remaining-tools loop processes tool_b"
    );
    assert!(
        snap.filter_saved_tokens > 0,
        "filter_saved_tokens must be > 0 after remaining-tools loop processes tool_b"
    );
}

// Regression test for issue #1910: corrections must be stored in user_corrections even when
// LearningConfig::enabled = false (skill auto-improvement is disabled).
#[tokio::test]
async fn correction_stored_when_learning_disabled() {
    use crate::config::LearningConfig;
    use std::sync::Arc;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_memory::semantic::SemanticMemory;

    let mock = MockProvider::default();
    let provider = AnyProvider::Mock(mock);
    let memory: SemanticMemory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        provider,
        "test-model",
    )
    .await
    .expect("in-memory SQLite must init");
    let memory = Arc::new(memory);

    let agent_provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let conv_id = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(agent_provider, channel, registry, None, 5, executor)
        .with_learning(LearningConfig {
            enabled: false,
            correction_detection: true,
            ..LearningConfig::default()
        })
        .with_memory(Arc::clone(&memory), conv_id, 20, 5, 10);

    // "no that's wrong" triggers ExplicitRejection (confidence 0.85 > default threshold 0.6)
    agent
        .detect_and_record_corrections("no that's wrong", Some(conv_id))
        .await;

    let rows = memory.sqlite().load_recent_corrections(10).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "correction must be stored even when learning is disabled"
    );
    assert_eq!(rows[0].correction_kind, "explicit_rejection");
    assert_eq!(rows[0].correction_text, "no that's wrong");
}

#[test]
fn test_scheduled_task_injection_format() {
    let prompt = "bash -c 'echo hello'";
    let text = format!("{}{prompt}", crate::agent::SCHEDULED_TASK_PREFIX);
    assert!(text.starts_with(crate::agent::SCHEDULED_TASK_PREFIX));
    assert!(text.contains(prompt));
}
