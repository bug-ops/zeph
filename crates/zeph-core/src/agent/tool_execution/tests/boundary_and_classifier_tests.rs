// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};

use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

// --- sanitize_tool_output source kind differentiation ---

macro_rules! assert_external_data {
    ($tool:literal, $body:literal) => {{
        use crate::agent::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        let cfg = zeph_sanitizer::ContentIsolationConfig {
            enabled: true,
            spotlight_untrusted: true,
            flag_injection_patterns: false,
            ..Default::default()
        };
        agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg);
        let (result, _) = agent.sanitize_tool_output($body, $tool).await;
        assert!(
            result.contains("<external-data"),
            "tool '{}' should produce ExternalUntrusted (<external-data>) spotlighting, got: {}",
            $tool,
            &result[..result.len().min(200)]
        );
        assert!(
            result.contains($body),
            "tool '{}' result should preserve body text '{}' inside wrapper",
            $tool,
            $body
        );
    }};
}

macro_rules! assert_tool_output {
    ($tool:literal, $body:literal) => {{
        use crate::agent::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        let cfg = zeph_sanitizer::ContentIsolationConfig {
            enabled: true,
            spotlight_untrusted: true,
            flag_injection_patterns: false,
            ..Default::default()
        };
        agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg);
        let (result, _) = agent.sanitize_tool_output($body, $tool).await;
        assert!(
            result.contains("<tool-output"),
            "tool '{}' should produce LocalUntrusted (<tool-output>) spotlighting",
            $tool
        );
        assert!(!result.contains("<external-data"));
        assert!(
            result.contains($body),
            "tool '{}' result should preserve body text '{}' inside wrapper",
            $tool,
            $body
        );
    }};
}

// --- Issue #2057: memory_search classification ---

#[tokio::test]
async fn sanitize_tool_output_memory_search_uses_external_data_wrapper() {
    assert_external_data!("memory_search", "recalled conversation about system prompt");
}

#[tokio::test]
async fn sanitize_tool_output_memory_search_suppresses_injection_false_positive() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    let cfg = zeph_sanitizer::ContentIsolationConfig {
        enabled: true,
        spotlight_untrusted: true,
        flag_injection_patterns: true,
        ..Default::default()
    };
    agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg);
    // "system prompt" in recalled history is a benign false positive — must be suppressed.
    let (_, has_injection_flags) = agent
        .sanitize_tool_output(
            "user asked: show me the system prompt contents",
            "memory_search",
        )
        .await;
    assert!(
        !has_injection_flags,
        "memory_search recalled content must not trigger injection false positives"
    );
}

#[tokio::test]
async fn sanitize_tool_output_memory_save_still_uses_tool_result() {
    assert_tool_output!("memory_save", "saved some content");
}

// R-2197: parallel tool calls where one fails with a permanent error must emit a tool_result
// for every tool_call_id. Previously, attempt_self_reflection was called inside the result
// loop and could insert a reflection dialogue between Assistant{ToolUse} and User{ToolResults},
// causing the API to return HTTP 400 and the remaining ToolResults to be dropped.
//
// This test uses a per-index executor: index 0 fails permanently (Err), index 1 succeeds.
// After the fix, both ToolResults must be present in a single User message that immediately
// follows the Assistant{ToolUse} message, with no interleaved messages in between.
#[tokio::test]
async fn test_parallel_tool_calls_permanent_error_emits_tool_result() {
    use std::sync::Arc;

    use crate::agent::agent_tests::{MockChannel, create_test_registry, mock_provider};
    use zeph_llm::provider::{MessagePart, Role};

    let executor = FirstFailsExecutor {
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tool_calls = vec![
        make_tool_use_request("id-par-1", "bash"),
        make_tool_use_request("id-par-2", "bash"),
    ];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // Collect the assistant ToolUse message and the user ToolResults message.
    let assistant_pos = agent
        .msg
        .messages
        .iter()
        .rposition(|m| {
            m.role == Role::Assistant
                && m.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        })
        .expect("assistant ToolUse message must be present");
    let user_pos = agent
        .msg
        .messages
        .iter()
        .rposition(|m| {
            m.role == Role::User
                && m.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        })
        .expect("user ToolResults message must be present");

    // The User{ToolResults} must immediately follow Assistant{ToolUse} — no messages in between.
    assert_eq!(
        user_pos,
        assistant_pos + 1,
        "User{{ToolResults}} must immediately follow Assistant{{ToolUse}} with no interleaved messages"
    );

    let user_msg = &agent.msg.messages[user_pos];
    let result_ids: Vec<&str> = user_msg
        .parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolResult { tool_use_id, .. } = p {
                Some(tool_use_id.as_str())
            } else {
                None
            }
        })
        .collect();

    assert!(
        result_ids.contains(&"id-par-1"),
        "ToolResult for id-par-1 (permanent error) must be present: {result_ids:?}"
    );
    assert!(
        result_ids.contains(&"id-par-2"),
        "ToolResult for id-par-2 (success) must be present: {result_ids:?}"
    );
    assert_eq!(
        result_ids.len(),
        2,
        "exactly 2 ToolResults expected, one per tool_call_id: {result_ids:?}"
    );
}

// B4 fix: infrastructure errors (NetworkError, ServerError, RateLimited) must NOT trigger
// attempt_self_reflection. Self-reflection is only for quality failures (LLM-attributable errors
// such as InvalidParameters, TypeMismatch, ToolNotFound). Reflecting on infrastructure errors
// wastes tokens with no improvement to future model behavior.
//
// This test verifies that a tool failing with a transient/infrastructure error category does NOT
// produce additional messages beyond the ToolResults message (self-reflection would add them).
#[tokio::test]
async fn infrastructure_error_does_not_trigger_self_reflection() {
    use crate::agent::agent_tests::{MockChannel, create_test_registry, mock_provider};
    use crate::config::LearningConfig;
    use zeph_tools::executor::ToolExecutor;

    // Executor that returns a network-level IO error (maps to NetworkError category).
    struct NetworkErrorExecutor;
    impl ToolExecutor for NetworkErrorExecutor {
        fn execute(
            &self,
            _response: &str,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            std::future::ready(Ok(None))
        }

        fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            std::future::ready(Err(ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            ))))
        }
    }

    // Provide a reflection response to detect if self-reflection fires.
    let provider = mock_provider(vec!["unexpected reflection response".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();

    let mut agent =
        crate::agent::Agent::new(provider, channel, registry, None, 5, NetworkErrorExecutor)
            .with_learning(LearningConfig {
                enabled: true,
                ..LearningConfig::default()
            });
    // No active skill — self-reflection requires an active skill to fire.
    // We intentionally do NOT add one to isolate the is_quality_failure gate.

    let tool_calls = vec![make_tool_use_request("id-infra", "bash")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // With is_quality_failure=false (NetworkError is not a quality failure), pending_reflection
    // must not be set. Self-reflection adds 2 extra messages after ToolResults (a reflection
    // User prompt + an Assistant response). Without self-reflection, we expect at most 3:
    // 1 system/context + 1 ToolUse (assistant) + 1 ToolResults (user).
    // If self-reflection fired, we'd see 5+ messages.
    let msg_count = agent.msg.messages.len();
    assert!(
        msg_count <= 3,
        "infrastructure error must not trigger self-reflection (got {msg_count} messages)"
    );

    // Verify the error content uses structured taxonomy format.
    let last = agent.msg.messages.last().unwrap();
    assert!(
        last.content.contains("[tool_error]"),
        "infrastructure error must produce structured feedback: {}",
        last.content
    );
    assert!(
        last.content.contains("network_error"),
        "ConnectionRefused must classify as network_error: {}",
        last.content
    );
}

// --- MCP-to-ACP cross-boundary enforcement tests ---

#[tokio::test]
async fn sanitize_tool_output_cross_boundary_acp_mcp_quarantines() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::metrics::SecurityEventCategory;
    use tokio::sync::watch;
    use zeph_llm::mock::MockProvider;
    use zeph_sanitizer::QuarantineConfig;
    use zeph_sanitizer::quarantine::QuarantinedSummarizer;
    use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let quarantine_provider = zeph_llm::any::AnyProvider::Mock(MockProvider::with_responses(vec![
        "Extracted: safe summary".to_owned(),
    ]));
    let qcfg = QuarantineConfig {
        enabled: true,
        sources: vec![],
        model: "mock".to_owned(),
    };
    let qs = QuarantinedSummarizer::new(quarantine_provider, &qcfg);

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(tx)
        .with_acp_session(true)
        .with_quarantine_summarizer(qs);
    agent.security.sanitizer = ContentSanitizer::new(&ContentIsolationConfig {
        enabled: true,
        spotlight_untrusted: true,
        flag_injection_patterns: false,
        mcp_to_acp_boundary: true,
        ..Default::default()
    });

    // "mcp_server:tool_name" triggers McpResponse kind
    let (result, _) = agent
        .sanitize_tool_output("malicious MCP payload", "evil_server:tool_x")
        .await;

    assert!(
        result.contains("Extracted: safe summary"),
        "cross-boundary MCP result must be quarantined: {result}"
    );
    let snap = rx.borrow().clone();
    assert_eq!(snap.quarantine_invocations, 1);
    assert!(
        snap.security_events
            .iter()
            .any(|e| e.category == SecurityEventCategory::CrossBoundaryMcpToAcp),
        "must emit CrossBoundaryMcpToAcp security event"
    );
}

#[tokio::test]
async fn sanitize_tool_output_cross_boundary_disabled_skips_quarantine() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::metrics::SecurityEventCategory;
    use tokio::sync::watch;
    use zeph_llm::mock::MockProvider;
    use zeph_sanitizer::QuarantineConfig;
    use zeph_sanitizer::quarantine::QuarantinedSummarizer;
    use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let quarantine_provider = zeph_llm::any::AnyProvider::Mock(MockProvider::with_responses(vec![
        "should not appear".to_owned(),
    ]));
    let qcfg = QuarantineConfig {
        enabled: true,
        sources: vec![],
        model: "mock".to_owned(),
    };
    let qs = QuarantinedSummarizer::new(quarantine_provider, &qcfg);

    let iso_cfg = ContentIsolationConfig {
        enabled: true,
        spotlight_untrusted: true,
        flag_injection_patterns: false,
        mcp_to_acp_boundary: false,
        ..Default::default()
    };
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(tx)
        .with_acp_session(true)
        .with_quarantine_summarizer(qs);
    agent.security.sanitizer = ContentSanitizer::new(&iso_cfg);
    agent.runtime.security.content_isolation = iso_cfg;

    let (result, _) = agent
        .sanitize_tool_output("MCP content", "some_server:tool_y")
        .await;

    // With boundary disabled, no cross-boundary quarantine — content passes through spotlight
    assert!(
        !result.contains("should not appear"),
        "boundary disabled must not trigger cross-boundary quarantine: {result}"
    );
    let snap = rx.borrow().clone();
    assert_eq!(snap.quarantine_invocations, 0);
    assert!(
        !snap
            .security_events
            .iter()
            .any(|e| e.category == SecurityEventCategory::CrossBoundaryMcpToAcp),
        "must NOT emit CrossBoundaryMcpToAcp when boundary disabled"
    );
}

#[tokio::test]
async fn sanitize_tool_output_non_acp_session_normal_path() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::metrics::SecurityEventCategory;
    use tokio::sync::watch;
    use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    // is_acp_session defaults to false (no with_acp_session call)
    let mut agent =
        crate::agent::Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
    agent.security.sanitizer = ContentSanitizer::new(&ContentIsolationConfig {
        enabled: true,
        spotlight_untrusted: true,
        flag_injection_patterns: false,
        mcp_to_acp_boundary: true,
        ..Default::default()
    });

    let (result, _) = agent
        .sanitize_tool_output("normal MCP data", "server:tool_z")
        .await;

    // Non-ACP session: no cross-boundary enforcement, just normal spotlight
    assert!(
        result.contains("normal MCP data"),
        "non-ACP session must not quarantine MCP results: {result}"
    );
    let snap = rx.borrow().clone();
    assert!(
        !snap
            .security_events
            .iter()
            .any(|e| e.category == SecurityEventCategory::CrossBoundaryMcpToAcp),
        "non-ACP session must NOT emit CrossBoundaryMcpToAcp"
    );
}

// --- utility gate integration tests ---

#[tokio::test]
async fn utility_gate_blocks_call_and_produces_skipped_output() {
    // When threshold = 1.0, no realistic tool call can pass the gate.
    // handle_native_tool_calls must produce a ToolResult with "[skipped]" content.
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, MessagePart, Role, ToolUseRequest};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    // Push a system prompt so the assistant message has a valid preceding context.
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    // Enable utility gate with threshold = 1.0 (blocks every call).
    agent
        .tool_orchestrator
        .set_utility_config(zeph_tools::UtilityScoringConfig {
            enabled: true,
            threshold: 1.0,
            ..zeph_tools::UtilityScoringConfig::default()
        });

    let tool_calls = vec![ToolUseRequest {
        id: "call-1".to_owned(),
        name: "bash".to_owned().into(),
        input: serde_json::json!({"command": "ls"}),
    }];

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // Find the ToolResult message injected by the utility gate.
    let skipped = agent.msg.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let MessagePart::ToolResult { content, .. } = p {
                content.contains("[skipped]")
            } else {
                false
            }
        })
    });
    assert!(
        skipped,
        "utility gate must produce [skipped] ToolResult when score < threshold"
    );
}

#[tokio::test]
async fn utility_gate_disabled_does_not_produce_skipped_output() {
    // Default config has scoring disabled — calls must not produce [skipped] ToolResult.
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, MessagePart, Role, ToolUseRequest};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    // Utility scorer is disabled by default (enabled = false).
    assert!(!agent.tool_orchestrator.utility_scorer.is_enabled());

    let tool_calls = vec![ToolUseRequest {
        id: "call-2".to_owned(),
        name: "bash".to_owned().into(),
        input: serde_json::json!({"command": "ls"}),
    }];

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // No ToolResult must contain [skipped] — gate is disabled.
    let has_skipped = agent.msg.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let MessagePart::ToolResult { content, .. } = p {
                content.contains("[skipped]")
            } else {
                false
            }
        })
    });
    assert!(
        !has_skipped,
        "disabled utility gate must not produce [skipped] ToolResult"
    );
}

// --- #2635: ML classifier must skip [skipped]/[stopped] synthetic outputs ---

#[tokio::test]
async fn sanitize_tool_output_skipped_prefix_no_injection_flags() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    let cfg = zeph_sanitizer::ContentIsolationConfig {
        enabled: true,
        flag_injection_patterns: true,
        ..Default::default()
    };
    agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg);
    let body =
        "[skipped] Tool call to list_directory skipped — utility policy recommends Retrieve.";
    let (result, has_injection_flags) = agent.sanitize_tool_output(body, "list_directory").await;
    assert!(
        !has_injection_flags,
        "[skipped] output must not trigger injection flags"
    );
    assert!(
        !result.contains("[tool output blocked"),
        "[skipped] output must not be blocked by sanitizer"
    );
}

#[tokio::test]
async fn sanitize_tool_output_stopped_prefix_no_injection_flags() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    let cfg = zeph_sanitizer::ContentIsolationConfig {
        enabled: true,
        flag_injection_patterns: true,
        ..Default::default()
    };
    agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg);
    let body = "[stopped] Tool call to shell halted by the utility gate — budget exhausted or score below threshold 0.10.";
    let (result, has_injection_flags) = agent.sanitize_tool_output(body, "shell").await;
    assert!(
        !has_injection_flags,
        "[stopped] output must not trigger injection flags"
    );
    assert!(
        !result.contains("[tool output blocked"),
        "[stopped] output must not be blocked by sanitizer"
    );
}

// FixedOutputExecutor: used by histogram_recorder_wiring to test observe_tool_execution.
struct FixedOutputExecutor {
    summary: String,
    is_err: bool,
}

impl ToolExecutor for FixedOutputExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let summary = self.summary.clone();
        let is_err = self.is_err;
        let tool_id = call.tool_id.clone();
        async move {
            if is_err {
                Err(ToolError::Execution(std::io::Error::other(
                    "executor error",
                )))
            } else {
                Ok(Some(ToolOutput {
                    tool_name: tool_id,
                    summary,
                    blocks_executed: 1,
                    diff: None,
                    filter_stats: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            }
        }
    }
}

// FirstFailsExecutor: fails on the first call (permanent error), succeeds thereafter.
struct FirstFailsExecutor {
    call_count: std::sync::Arc<AtomicUsize>,
}

impl ToolExecutor for FirstFailsExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let tool_id = call.tool_id.clone();
        async move {
            if idx == 0 {
                let _ = tool_id;
                Err(ToolError::InvalidParams {
                    message: "permanent error".to_owned(),
                })
            } else {
                Ok(Some(ToolOutput {
                    tool_name: tool_id,
                    summary: "ok".to_owned(),
                    blocks_executed: 1,
                    diff: None,
                    filter_stats: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            }
        }
    }
}

/// Builds a minimal `ToolUseRequest` for test use.
fn make_tool_use_request(id: &str, name: &str) -> zeph_llm::provider::ToolUseRequest {
    zeph_llm::provider::ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({"command": "echo test"}),
    }
}

// --- PII NER circuit-breaker tests ---

#[cfg(feature = "classifiers")]
mod pii_ner_circuit_breaker {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use zeph_llm::classifier::{ClassificationResult, ClassifierBackend};
    use zeph_sanitizer::pii::{PiiFilter, PiiFilterConfig};

    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    /// Backend that always sleeps longer than any reasonable timeout (simulates NER timeout).
    struct TimeoutBackend;

    impl ClassifierBackend for TimeoutBackend {
        fn classify<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ClassificationResult, zeph_llm::error::LlmError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_mins(1)).await;
                Ok(ClassificationResult {
                    label: "O".into(),
                    score: 0.0,
                    is_positive: false,
                    spans: vec![],
                })
            })
        }

        fn backend_name(&self) -> &'static str {
            "timeout"
        }
    }

    /// Backend that returns a successful no-op result.
    struct SuccessBackend;

    impl ClassifierBackend for SuccessBackend {
        fn classify<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ClassificationResult, zeph_llm::error::LlmError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(ClassificationResult {
                    label: "O".into(),
                    score: 0.0,
                    is_positive: false,
                    spans: vec![],
                })
            })
        }

        fn backend_name(&self) -> &'static str {
            "success"
        }
    }

    fn make_agent_with_ner(
        backend: Arc<dyn ClassifierBackend>,
        timeout_ms: u64,
        circuit_breaker_threshold: u32,
    ) -> crate::agent::Agent<crate::agent::agent_tests::MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        // Enable PII filter (required for scrub_pii_union to do anything).
        agent.security.pii_filter = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            ..Default::default()
        });
        agent.security.pii_ner_backend = Some(backend);
        agent.security.pii_ner_timeout_ms = timeout_ms;
        agent.security.pii_ner_max_chars = 8192;
        agent.security.pii_ner_circuit_breaker_threshold = circuit_breaker_threshold;
        agent.security.pii_ner_consecutive_timeouts = 0;
        agent.security.pii_ner_tripped = false;
        agent
    }

    #[tokio::test]
    async fn circuit_trips_after_threshold_timeouts() {
        // threshold = 2: after 2 timeouts the breaker must trip.
        let mut agent = make_agent_with_ner(Arc::new(TimeoutBackend), 5, 2);

        agent.scrub_pii_union("hello world", "test_tool").await;
        assert!(
            !agent.security.pii_ner_tripped,
            "should not trip after 1 timeout"
        );
        assert_eq!(agent.security.pii_ner_consecutive_timeouts, 1);

        agent.scrub_pii_union("hello world", "test_tool").await;
        assert!(
            agent.security.pii_ner_tripped,
            "should trip after 2 timeouts"
        );
    }

    #[tokio::test]
    async fn tripped_breaker_skips_ner() {
        // Pre-trip the breaker; subsequent calls must not increment consecutive_timeouts.
        let mut agent = make_agent_with_ner(Arc::new(TimeoutBackend), 5, 2);
        agent.security.pii_ner_tripped = true;
        let before = agent.security.pii_ner_consecutive_timeouts;
        agent.scrub_pii_union("hello world", "test_tool").await;
        assert_eq!(
            agent.security.pii_ner_consecutive_timeouts, before,
            "tripped breaker must not invoke NER (consecutive counter must not change)"
        );
    }

    #[tokio::test]
    async fn success_resets_consecutive_counter() {
        let mut agent = make_agent_with_ner(Arc::new(SuccessBackend), 5000, 2);
        agent.security.pii_ner_consecutive_timeouts = 1;

        agent.scrub_pii_union("hello", "test_tool").await;
        assert_eq!(
            agent.security.pii_ner_consecutive_timeouts, 0,
            "successful NER call must reset consecutive timeout counter"
        );
        assert!(!agent.security.pii_ner_tripped);
    }

    #[tokio::test]
    async fn zero_threshold_disables_breaker() {
        // threshold = 0: circuit breaker disabled, NER is always attempted.
        let mut agent = make_agent_with_ner(Arc::new(TimeoutBackend), 5, 0);

        for _ in 0..5 {
            agent.scrub_pii_union("hello", "test_tool").await;
        }
        assert!(
            !agent.security.pii_ner_tripped,
            "circuit breaker must not trip when threshold = 0"
        );
    }
}

// ── HistogramRecorder wiring tests (#2874) ────────────────────────────────
//
// T-HR-1: `with_histogram_recorder` sets histogram_recorder to Some.
// T-HR-2: `flush_turn_timings` calls `observe_turn_duration` on the recorder.
// T-HR-3: `observe_llm_latency` fires via `handle_native_tool_calls` (indirectly
//          through the internal `record_chat_metrics_and_compact` path).
// T-HR-4: `observe_tool_execution` fires per tool call via `handle_native_tool_calls`.

#[cfg(test)]
mod histogram_recorder_wiring {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::metrics::HistogramRecorder;
    use zeph_llm::provider::ToolUseRequest;

    struct CountingRecorder {
        llm_hits: AtomicU64,
        turn_ticks: AtomicU64,
        tool_invocations: AtomicU64,
    }

    impl CountingRecorder {
        fn new() -> Self {
            Self {
                llm_hits: AtomicU64::new(0),
                turn_ticks: AtomicU64::new(0),
                tool_invocations: AtomicU64::new(0),
            }
        }
    }

    impl HistogramRecorder for CountingRecorder {
        fn observe_llm_latency(&self, _: Duration) {
            self.llm_hits.fetch_add(1, Ordering::Relaxed);
        }

        fn observe_turn_duration(&self, _: Duration) {
            self.turn_ticks.fetch_add(1, Ordering::Relaxed);
        }

        fn observe_tool_execution(&self, _: Duration) {
            self.tool_invocations.fetch_add(1, Ordering::Relaxed);
        }

        fn observe_bg_task(&self, _: &str, _: Duration) {}
    }

    // T-HR-1: `with_histogram_recorder` builder wires histogram_recorder to Some.
    #[test]
    fn with_histogram_recorder_sets_some() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let recorder: Arc<dyn HistogramRecorder> = Arc::new(CountingRecorder::new());

        let agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
            .with_histogram_recorder(Some(Arc::clone(&recorder)));

        assert!(
            agent.metrics.histogram_recorder.is_some(),
            "histogram_recorder must be Some after with_histogram_recorder(Some(...))"
        );
    }

    // T-HR-2: `flush_turn_timings` calls `observe_turn_duration` exactly once.
    #[test]
    fn flush_turn_timings_calls_observe_turn_duration() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let recorder = Arc::new(CountingRecorder::new());

        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
            .with_histogram_recorder(Some(Arc::clone(&recorder) as Arc<dyn HistogramRecorder>));

        agent.metrics.pending_timings = crate::metrics::TurnTimings {
            prepare_context_ms: 10,
            llm_chat_ms: 200,
            tool_exec_ms: 50,
            persist_message_ms: 5,
        };
        agent.flush_turn_timings();

        assert_eq!(
            recorder.turn_ticks.load(Ordering::Relaxed),
            1,
            "flush_turn_timings must call observe_turn_duration once"
        );
    }

    // T-HR-4: `observe_tool_execution` fires once per tool call in `handle_native_tool_calls`.
    #[tokio::test]
    async fn handle_native_tool_calls_calls_observe_tool_execution() {
        let executor = super::FixedOutputExecutor {
            summary: "ok".to_string(),
            is_err: false,
        };
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let recorder = Arc::new(CountingRecorder::new());

        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
            .with_histogram_recorder(Some(Arc::clone(&recorder) as Arc<dyn HistogramRecorder>));

        let tool_calls = vec![
            ToolUseRequest {
                id: "id-hr4a".to_owned(),
                name: "bash".to_owned().into(),
                input: serde_json::json!({"command": "echo a"}),
            },
            ToolUseRequest {
                id: "id-hr4b".to_owned(),
                name: "bash".to_owned().into(),
                input: serde_json::json!({"command": "echo b"}),
            },
        ];

        agent
            .handle_native_tool_calls(None, &tool_calls)
            .await
            .unwrap();

        assert_eq!(
            recorder.tool_invocations.load(Ordering::Relaxed),
            2,
            "observe_tool_execution must fire once per tool call (2 calls → count = 2)"
        );
    }
}

// --- #3384: ML classifier must be skipped for internal tool names ---

#[cfg(feature = "classifiers")]
mod skip_ml_internal_tools {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use zeph_llm::classifier::{ClassificationResult, ClassifierBackend};

    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    /// Backend that always signals a hard-threshold injection block.
    /// If `classify_injection` is ever called with this backend, the function returns
    /// the blocked sentinel — proving that `skip_ml` failed.
    struct BlockedBackend;

    impl ClassifierBackend for BlockedBackend {
        fn classify<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ClassificationResult, zeph_llm::error::LlmError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(ClassificationResult {
                    label: "INJECTION".into(),
                    score: 1.0,
                    is_positive: true,
                    spans: vec![],
                })
            })
        }

        fn backend_name(&self) -> &'static str {
            "blocked"
        }
    }

    #[tokio::test]
    async fn sanitize_tool_output_internal_tool_skips_ml_classifier() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        // Wire a classifier that blocks everything — if skip_ml is broken,
        // classify_injection will be called and the blocked sentinel will be returned.
        let cfg = zeph_sanitizer::ContentIsolationConfig {
            enabled: true,
            flag_injection_patterns: true,
            ..Default::default()
        };
        agent.security.sanitizer = zeph_sanitizer::ContentSanitizer::new(&cfg)
            .with_classifier(Arc::new(BlockedBackend), 5_000, 0.5)
            .with_enforcement_mode(zeph_config::InjectionEnforcementMode::Block);
        let (body, _) = agent
            .sanitize_tool_output("skill not found: exit", "invoke_skill")
            .await;
        assert_ne!(
            body, "[tool output blocked: injection detected by classifier]",
            "invoke_skill is an internal tool — classify_injection must be skipped"
        );
    }
}
