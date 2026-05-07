// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role};

use crate::agent::Agent;
use crate::agent::tests::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::metrics::HistogramRecorder;

fn make_agent() -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent.services.focus.config.enabled = true;
    // System prompt at index 0 (required by complete_focus insert logic)
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent
}

/// Helper: call `handle_focus_tool` and flush the pending checkpoint into agent history,
/// simulating the deferred insertion that `execute_tool_calls_batch` performs (#3262).
fn call_focus_tool(
    agent: &mut Agent<MockChannel>,
    tool_name: &str,
    input: &serde_json::Value,
) -> String {
    let (result, maybe_checkpoint) = agent.handle_focus_tool(tool_name, input);
    if let Some(cp) = maybe_checkpoint {
        agent.push_message(cp);
    }
    result
}

#[test]
fn start_focus_happy_path_inserts_pinned_checkpoint() {
    let mut agent = make_agent();
    let input = serde_json::json!({"scope": "reading auth files"});
    let result = call_focus_tool(&mut agent, "start_focus", &input);

    assert!(
        !result.starts_with("[error]"),
        "start_focus must not return error: {result}"
    );
    assert!(
        agent.services.focus.is_active(),
        "focus session must be active after start_focus"
    );

    // Checkpoint message must exist and be pinned (S5 fix)
    let checkpoint = agent
        .msg
        .messages
        .iter()
        .find(|m| m.metadata.focus_marker_id.is_some());
    assert!(checkpoint.is_some(), "checkpoint message must be inserted");
    let checkpoint = checkpoint.unwrap();
    assert!(
        checkpoint.metadata.focus_pinned,
        "checkpoint message must have focus_pinned=true (S5 fix)"
    );
}

#[test]
fn start_focus_checkpoint_inserted_after_tool_result() {
    // Verify that when the deferred pattern is used, the checkpoint lands AFTER
    // the tool-result User message, maintaining valid OpenAI ordering (#3262).
    let mut agent = make_agent();

    // Simulate assistant message with tool call already in history
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: String::new(),
        parts: vec![MessagePart::ToolUse {
            id: "call_test_1".to_string(),
            name: "start_focus".to_string(),
            input: serde_json::json!({"scope": "test"}),
        }],
        metadata: zeph_llm::provider::MessageMetadata::default(),
    });

    // Capture pending checkpoint WITHOUT flushing it yet
    let (result, maybe_checkpoint) =
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "test"}));
    assert!(!result.starts_with("[error]"));
    assert!(
        maybe_checkpoint.is_some(),
        "start_focus must return a pending checkpoint"
    );

    // Simulate push_message(user_msg) for tool result — happens before checkpoint
    let tool_result_msg = Message {
        role: Role::User,
        content: String::new(),
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "call_test_1".to_string(),
            content: result.clone(),
            is_error: false,
        }],
        metadata: zeph_llm::provider::MessageMetadata::default(),
    };
    agent.msg.messages.push(tool_result_msg);

    // Now flush checkpoint — must land after tool result
    if let Some(cp) = maybe_checkpoint {
        agent.push_message(cp);
    }

    let tool_result_pos = agent.msg.messages.iter().position(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }))
    });
    let checkpoint_pos = agent
        .msg
        .messages
        .iter()
        .position(|m| m.metadata.focus_marker_id.is_some());
    assert!(tool_result_pos.is_some(), "tool result must be in history");
    assert!(checkpoint_pos.is_some(), "checkpoint must be in history");
    assert!(
        tool_result_pos.unwrap() < checkpoint_pos.unwrap(),
        "tool result (pos={}) must precede checkpoint (pos={})",
        tool_result_pos.unwrap(),
        checkpoint_pos.unwrap()
    );
}

#[test]
fn start_focus_errors_when_already_active() {
    let mut agent = make_agent();
    call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "first"}),
    );
    let result = call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "second"}),
    );
    assert!(
        result.starts_with("[error]"),
        "second start_focus must return error: {result}"
    );
}

#[test]
fn complete_focus_errors_when_no_active_session() {
    let mut agent = make_agent();
    let result = call_focus_tool(
        &mut agent,
        "complete_focus",
        &serde_json::json!({"summary": "done"}),
    );
    assert!(
        result.starts_with("[error]"),
        "complete_focus without active session must error: {result}"
    );
}

#[test]
fn complete_focus_happy_path_clears_session_and_appends_knowledge() {
    let mut agent = make_agent();
    call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "test"}),
    );
    // Add some messages in the focus window
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::User, "some work"));
    let result = call_focus_tool(
        &mut agent,
        "complete_focus",
        &serde_json::json!({"summary": "learned stuff"}),
    );
    assert!(
        !result.starts_with("[error]"),
        "complete_focus must not error: {result}"
    );
    assert!(
        !agent.services.focus.is_active(),
        "focus session must be cleared after complete_focus"
    );
    assert!(
        !agent.services.focus.knowledge_blocks.is_empty(),
        "knowledge must be appended"
    );
}

#[test]
fn complete_focus_marker_not_found_returns_error() {
    let mut agent = make_agent();
    call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "test"}),
    );
    // Remove checkpoint by hand to simulate marker eviction
    agent
        .msg
        .messages
        .retain(|m| m.metadata.focus_marker_id.is_none());
    let result = call_focus_tool(
        &mut agent,
        "complete_focus",
        &serde_json::json!({"summary": "done"}),
    );
    assert!(
        result.starts_with("[error]"),
        "must return error when checkpoint not found (S4): {result}"
    );
}

#[test]
fn complete_focus_truncates_bracketed_messages() {
    let mut agent = make_agent();
    call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "test"}),
    );
    let before_len = agent.msg.messages.len();
    // Add 3 messages in the focus window
    for i in 0..3 {
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, format!("msg {i}")));
    }
    call_focus_tool(
        &mut agent,
        "complete_focus",
        &serde_json::json!({"summary": "done"}),
    );
    // Messages after complete_focus: [system prompt, knowledge block] at minimum
    // Checkpoint + bracketed messages must be gone
    assert!(
        agent.msg.messages.len() < before_len + 3,
        "bracketed messages must be truncated after complete_focus"
    );
}

/// Regression test for #3476: when `complete_focus` is called in a batch with other
/// tools, the current turn's assistant `tool_calls` message must be preserved after
/// truncation so the subsequent tool results have a valid parent.
#[test]
fn complete_focus_in_batch_preserves_current_turn_assistant_message() {
    let mut agent = make_agent();
    call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "test"}),
    );
    // Simulate a mixed batch: push a bracketed message inside the focus window...
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::User, "some work"));
    // ...then simulate the agent pushing the current-turn assistant message
    // (containing ToolUse parts for [read, complete_focus]) before preprocess runs.
    let batch_assistant = Message::from_parts(
        Role::Assistant,
        vec![
            MessagePart::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": "/tmp/x"}),
            },
            MessagePart::ToolUse {
                id: "call-2".to_string(),
                name: "complete_focus".to_string(),
                input: serde_json::json!({"summary": "done"}),
            },
        ],
    );
    agent.push_message(batch_assistant);

    // Now call complete_focus (as preprocess_focus_compress_calls would).
    let result = call_focus_tool(
        &mut agent,
        "complete_focus",
        &serde_json::json!({"summary": "learned stuff"}),
    );
    assert!(
        !result.starts_with("[error]"),
        "complete_focus must not error: {result}"
    );

    // The current-turn assistant message must still be the last assistant message
    // so that the upcoming tool results have a valid parent.
    let last_assistant = agent
        .msg
        .messages
        .iter()
        .rfind(|m| m.role == Role::Assistant);
    assert!(
        last_assistant.is_some(),
        "current-turn assistant message must be preserved after truncation (#3476)"
    );
    let last_assistant = last_assistant.unwrap();
    assert!(
        last_assistant
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { .. })),
        "preserved assistant message must have ToolUse parts"
    );
}

#[test]
fn min_messages_per_focus_guard_not_enforced_in_tool() {
    // The guard for min_messages_per_focus is advisory (reminder injection path).
    // handle_focus_tool itself does not enforce it — the LLM decides when to call.
    let mut agent = make_agent();
    agent.services.focus.config.min_messages_per_focus = 100; // very high, but tool doesn't check
    let result = call_focus_tool(
        &mut agent,
        "start_focus",
        &serde_json::json!({"scope": "x"}),
    );
    assert!(
        !result.starts_with("[error]"),
        "tool must not enforce min_messages_per_focus: {result}"
    );
}

// --- utility gate integration ---

#[test]
fn utility_gate_disabled_by_default_scorer_is_not_enabled() {
    // The default ToolOrchestrator has scoring disabled — no calls are gated.
    let agent = make_agent();
    assert!(
        !agent.tool_orchestrator.utility_scorer.is_enabled(),
        "utility scorer must be disabled by default"
    );
}

#[test]
fn set_utility_config_enables_scorer_on_agent() {
    // set_utility_config wires the scorer into the tool orchestrator (integration path).
    let mut agent = make_agent();
    agent
        .tool_orchestrator
        .set_utility_config(zeph_tools::UtilityScoringConfig {
            enabled: true,
            threshold: 0.5,
            ..zeph_tools::UtilityScoringConfig::default()
        });
    assert!(
        agent.tool_orchestrator.utility_scorer.is_enabled(),
        "scorer must be enabled after set_utility_config"
    );
    assert!(
        (agent.tool_orchestrator.utility_scorer.threshold() - 0.5).abs() < f32::EPSILON,
        "threshold must match config"
    );
}

#[test]
fn clear_utility_state_resets_per_turn_redundancy_tracking() {
    // Verify that clear_utility_state() clears the redundancy state so the
    // next turn treats all calls as fresh (no stale redundancy carry-over).
    use zeph_tools::{ToolCall, UtilityContext};

    let mut agent = make_agent();
    agent
        .tool_orchestrator
        .set_utility_config(zeph_tools::UtilityScoringConfig {
            enabled: true,
            threshold: 0.0,
            ..zeph_tools::UtilityScoringConfig::default()
        });

    let call = ToolCall {
        tool_id: zeph_common::ToolName::new("bash"),
        params: serde_json::Map::new(),
        caller_id: None,
        context: None,
    };
    let ctx = UtilityContext {
        tool_calls_this_turn: 0,
        tokens_consumed: 0,
        token_budget: 1000,
        user_requested: false,
    };

    // Record the call to create redundancy state.
    agent.tool_orchestrator.utility_scorer.record_call(&call);

    // Before clear: redundancy is 1.0.
    let score_before = agent
        .tool_orchestrator
        .utility_scorer
        .score(&call, &ctx)
        .unwrap();
    assert!(
        (score_before.redundancy - 1.0).abs() < f32::EPSILON,
        "redundancy must be 1.0 before clear"
    );

    // clear_utility_state simulates turn start.
    agent.tool_orchestrator.clear_utility_state();

    // After clear: redundancy is 0.0.
    let score_after = agent
        .tool_orchestrator
        .utility_scorer
        .score(&call, &ctx)
        .unwrap();
    assert!(
        score_after.redundancy.abs() < f32::EPSILON,
        "redundancy must be 0.0 after clear_utility_state"
    );
}

// --- explicit_request detection: parts vs content (#2641) ---

#[test]
fn explicit_request_detected_from_content_when_parts_empty() {
    // Text-only user messages are created via Message::from_legacy which sets
    // parts: vec![] and stores text only in content.  The fix ensures we read
    // content when parts is empty so the bypass fires correctly.
    use zeph_llm::provider::Message;
    let msg = Message::from_legacy(Role::User, "please call the list_directory tool");
    assert!(msg.parts.is_empty(), "from_legacy must produce empty parts");
    let text = if msg.parts.is_empty() {
        msg.content.clone()
    } else {
        msg.parts
            .iter()
            .filter_map(|p| {
                if let zeph_llm::provider::MessagePart::Text { text } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(
        zeph_tools::has_explicit_tool_request(&text),
        "explicit_request must be true when content contains tool request"
    );
}

#[test]
fn explicit_request_not_detected_from_empty_parts_without_tool_keyword() {
    use zeph_llm::provider::Message;
    let msg = Message::from_legacy(Role::User, "what is the weather today?");
    let text = if msg.parts.is_empty() {
        msg.content.clone()
    } else {
        msg.parts
            .iter()
            .filter_map(|p| {
                if let zeph_llm::provider::MessagePart::Text { text } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(
        !zeph_tools::has_explicit_tool_request(&text),
        "explicit_request must be false when content has no tool request"
    );
}

// T-HR-3: `record_chat_metrics_and_compact` calls `observe_llm_latency` on the recorder.
#[tokio::test]
async fn record_chat_metrics_calls_observe_llm_latency() {
    struct CountingRecorder {
        llm_count: AtomicU64,
    }

    impl HistogramRecorder for CountingRecorder {
        fn observe_llm_latency(&self, _: Duration) {
            self.llm_count.fetch_add(1, Ordering::Relaxed);
        }

        fn observe_turn_duration(&self, _: Duration) {}

        fn observe_tool_execution(&self, _: Duration) {}

        fn observe_bg_task(&self, _: &str, _: Duration) {}
    }

    let recorder = Arc::new(CountingRecorder {
        llm_count: AtomicU64::new(0),
    });

    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_histogram_recorder(Some(Arc::clone(&recorder) as Arc<dyn HistogramRecorder>));

    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    let start = Instant::now();
    let response = ChatResponse::Text("hello".to_owned());
    agent
        .record_chat_metrics_and_compact(start, &response)
        .await
        .unwrap();

    assert_eq!(
        recorder.llm_count.load(Ordering::Relaxed),
        1,
        "record_chat_metrics_and_compact must call observe_llm_latency once"
    );
}

// --- LSP hover injection path (#3595) ---

fn make_agent_with_lsp_note(note: &'static str) -> Agent<MockChannel> {
    use std::sync::Arc;
    let mut agent = Agent::new(
        mock_provider(vec![String::new()]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
    let manager = Arc::new(zeph_mcp::McpManager::new(vec![], vec![], enforcer));
    let mut lsp_runner = crate::lsp_hooks::LspHookRunner::new(
        manager,
        crate::lsp_hooks::LspConfig {
            enabled: true,
            token_budget: 500,
            ..crate::lsp_hooks::LspConfig::default()
        },
    );
    lsp_runner.push_note("hover", note, 5);
    agent.services.session.lsp_hooks = Some(lsp_runner);
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent
}

/// Regression test for #3595: LSP notes queued in `lsp_hooks.pending_notes` must be
/// injected as a `Role::System` message into `self.msg.messages` inside
/// `call_chat_with_tools`, before the LLM provider is called.
///
/// The old guard (`last_msg_has_tool_results`) was evaluated at the top of
/// `process_single_native_turn` on the *next* iteration, when tool results had
/// already been committed to history, so it always fired and prevented injection.
/// The fix moves injection unconditionally into `call_chat_with_tools`.
#[tokio::test]
async fn lsp_notes_injected_before_llm_call_in_call_chat_with_tools() {
    let mut agent = make_agent_with_lsp_note("fn foo() -> u32");

    let _ = agent.call_chat_with_tools(&[]).await;

    let lsp_msg = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::System && m.content.starts_with("[lsp "));
    assert!(
        lsp_msg.is_some(),
        "call_chat_with_tools must inject a [lsp hover] System message before the LLM call"
    );
    assert!(
        lsp_msg.unwrap().content.contains("fn foo() -> u32"),
        "injected LSP message must contain the queued note content"
    );
}

/// On a retry attempt the note queue is already empty (drained on the first call),
/// so `call_chat_with_tools` must remove the stale LSP message and not re-inject.
/// This verifies that notes never accumulate across retry iterations.
#[tokio::test]
async fn lsp_notes_not_duplicated_on_retry() {
    use zeph_llm::LlmError;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    // First call → ContextLengthExceeded, second call → success.
    let provider = AnyProvider::Mock(
        MockProvider::with_responses(vec![String::new()])
            .with_errors(vec![LlmError::ContextLengthExceeded]),
    );
    let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
    let manager = Arc::new(zeph_mcp::McpManager::new(vec![], vec![], enforcer));
    let mut lsp_runner = crate::lsp_hooks::LspHookRunner::new(
        manager,
        crate::lsp_hooks::LspConfig {
            enabled: true,
            token_budget: 500,
            ..crate::lsp_hooks::LspConfig::default()
        },
    );
    lsp_runner.push_note("hover", "fn bar() -> bool", 5);

    let mut agent = Agent::new(
        provider,
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent.services.session.lsp_hooks = Some(lsp_runner);
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent.context_manager.budget = Some(crate::context::ContextBudget::new(200_000, 0.20));

    let _ = agent.call_chat_with_tools_retry(&[], 2).await;

    let lsp_count = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::System && m.content.starts_with("[lsp "))
        .count();
    assert_eq!(
        lsp_count, 0,
        "after retry the stale LSP message must be removed and not re-injected \
        (queue was drained on first attempt)"
    );
}

// ── commit_speculative_tier unit tests (issues #3652, #3653) ─────────────────────────────

use zeph_common::ToolName;
use zeph_config::tools::{SpeculationMode, SpeculativeConfig};
use zeph_llm::provider::ToolUseRequest;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

use crate::agent::speculative::SpeculationEngine;
use crate::agent::speculative::prediction::{Prediction, PredictionSource};

struct AlwaysOkSpecExec;
impl ToolExecutor for AlwaysOkSpecExec {
    async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary: "speculative-ok".into(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))
    }

    fn is_tool_speculatable(&self, _: &str) -> bool {
        true
    }
}

struct AlwaysErrSpecExec;
impl ToolExecutor for AlwaysErrSpecExec {
    async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, _: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Execution(std::io::Error::other(
            "simulated error",
        )))
    }

    fn is_tool_speculatable(&self, _: &str) -> bool {
        true
    }
}

fn decoding_engine<E: ToolExecutor + 'static>(exec: E) -> Arc<SpeculationEngine> {
    Arc::new(SpeculationEngine::new(
        Arc::new(exec),
        SpeculativeConfig {
            mode: SpeculationMode::Decoding,
            ..Default::default()
        },
    ))
}

fn test_tool_call(tool_id: &str) -> ToolCall {
    ToolCall {
        tool_id: ToolName::new(tool_id),
        params: serde_json::Map::new(),
        caller_id: None,
        context: None,
    }
}

fn test_tool_use_request(name: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: format!("id-{name}"),
        name: ToolName::new(name),
        input: serde_json::Value::Object(serde_json::Map::new()),
    }
}

fn test_prediction(tool_id: &str) -> Prediction {
    Prediction {
        tool_id: ToolName::new(tool_id),
        args: serde_json::Map::new(),
        confidence: 0.9,
        source: PredictionSource::StreamPartial,
    }
}

/// `engine = None` → returns empty map immediately (zero-cost fast path).
#[tokio::test]
async fn commit_speculative_tier_no_engine_returns_empty() {
    let mut agent = make_agent();
    let calls = [test_tool_call("echo")];
    let tool_calls = [test_tool_use_request("echo")];
    let tool_call_ids = ["id-0".to_string()];
    let mut tool_started_ats = [Instant::now()];
    let before = tool_started_ats[0];

    let commits = agent
        .commit_speculative_tier(
            &[0],
            &calls,
            &tool_calls,
            &tool_call_ids,
            &mut tool_started_ats,
            None,
        )
        .await
        .expect("commit_speculative_tier must not fail with no engine");

    assert!(commits.is_empty(), "no engine → empty commit map");
    assert_eq!(
        tool_started_ats[0], before,
        "tool_started_ats must not be modified when engine is None"
    );
}

/// `try_commit` returns `None` for all calls (cache miss) → empty commit map.
#[tokio::test]
async fn commit_speculative_tier_cache_miss_returns_empty() {
    let engine = decoding_engine(AlwaysOkSpecExec);
    let mut agent = make_agent();
    let calls = [test_tool_call("echo")];
    let tool_calls = [test_tool_use_request("echo")];
    let tool_call_ids = ["id-0".to_string()];
    let mut tool_started_ats = [Instant::now()];

    // Nothing dispatched into the engine — every try_commit will be a miss.
    let commits = agent
        .commit_speculative_tier(
            &[0],
            &calls,
            &tool_calls,
            &tool_call_ids,
            &mut tool_started_ats,
            Some(&engine),
        )
        .await
        .expect("commit_speculative_tier must not fail on cache miss");

    assert!(commits.is_empty(), "cache miss → empty commit map");
}

/// `try_commit` returns `Ok(result)` → index in map, `tool_started_ats` stamped,
/// `ToolStartEvent { speculative: true }` emitted.
#[tokio::test]
async fn commit_speculative_tier_ok_result_stamps_and_emits_event() {
    let engine = decoding_engine(AlwaysOkSpecExec);
    let pred = test_prediction("echo");
    engine.try_dispatch(&pred, zeph_common::SkillTrustLevel::Trusted);

    // Let the speculative task complete.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut agent = make_agent();
    let calls = [test_tool_call("echo")];
    let tool_calls = [test_tool_use_request("echo")];
    let tool_call_ids = ["id-0".to_string()];
    let before = Instant::now();
    let mut tool_started_ats = [before];

    let commits = agent
        .commit_speculative_tier(
            &[0],
            &calls,
            &tool_calls,
            &tool_call_ids,
            &mut tool_started_ats,
            Some(&engine),
        )
        .await
        .expect("commit_speculative_tier must not fail on cache hit");

    assert!(
        commits.contains_key(&0),
        "committed index must be in the map"
    );
    assert!(
        commits[&0].is_ok(),
        "AlwaysOkSpecExec must produce Ok result"
    );
    assert!(
        tool_started_ats[0] >= before,
        "tool_started_ats[idx] must be stamped at or after before"
    );

    let starts = agent.channel.tool_starts.lock().unwrap();
    assert_eq!(
        starts.len(),
        1,
        "exactly one ToolStartEvent must be emitted"
    );
    assert!(
        starts[0].speculative,
        "ToolStartEvent.speculative must be true for committed speculative call"
    );
    assert_eq!(
        starts[0].tool_name.as_str(),
        "echo",
        "ToolStartEvent.tool_name must match the tool"
    );
}

/// `try_commit` returns `Err(_)` → index still in map with `Err`, `tracing::warn` fires.
#[tokio::test]
async fn commit_speculative_tier_err_result_still_in_map() {
    let engine = decoding_engine(AlwaysErrSpecExec);
    let pred = test_prediction("echo");
    engine.try_dispatch(&pred, zeph_common::SkillTrustLevel::Trusted);

    // Let the speculative task complete.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut agent = make_agent();
    let calls = [test_tool_call("echo")];
    let tool_calls = [test_tool_use_request("echo")];
    let tool_call_ids = ["id-0".to_string()];
    let mut tool_started_ats = [Instant::now()];

    let commits = agent
        .commit_speculative_tier(
            &[0],
            &calls,
            &tool_calls,
            &tool_call_ids,
            &mut tool_started_ats,
            Some(&engine),
        )
        .await
        .expect("commit_speculative_tier must not fail when committed result is Err");

    assert!(
        commits.contains_key(&0),
        "even an Err result must be in the commit map"
    );
    assert!(
        commits[&0].is_err(),
        "AlwaysErrSpecExec must produce Err result"
    );
}
