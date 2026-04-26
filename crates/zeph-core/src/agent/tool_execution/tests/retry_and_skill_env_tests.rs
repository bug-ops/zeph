// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod retry_tests {
    use crate::agent::agent_tests::*;
    use zeph_llm::LlmError;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    fn agent_with_provider(provider: AnyProvider) -> crate::agent::Agent<MockChannel> {
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent
    }

    #[tokio::test]
    async fn call_llm_with_retry_succeeds_on_first_attempt() {
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
        let mut agent = agent_with_provider(provider);
        let result = agent.call_llm_with_retry(2).await.unwrap();
        assert_eq!(result.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn call_llm_with_retry_recovers_after_context_length_error() {
        // First call returns ContextLengthExceeded, second succeeds.
        // compact_context() is a no-op with only 1 non-system message + system prompt,
        // but the retry logic itself must still re-call after compaction.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["recovered".into()])
                .with_errors(vec![LlmError::ContextLengthExceeded]),
        );
        let mut agent = agent_with_provider(provider);
        // Add context budget so compact_context can run
        agent.context_manager.budget = Some(zeph_core_budget_for_test());
        let result = agent.call_llm_with_retry(2).await.unwrap();
        assert_eq!(result.as_deref(), Some("recovered"));
    }

    fn zeph_core_budget_for_test() -> crate::context::ContextBudget {
        crate::context::ContextBudget::new(200_000, 0.20)
    }

    #[tokio::test]
    async fn call_llm_with_retry_propagates_non_context_error() {
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec![])
                .with_errors(vec![LlmError::Other("network error".into())]),
        );
        let mut agent = agent_with_provider(provider);
        let result: Result<Option<String>, _> = agent.call_llm_with_retry(2).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_context_length_error());
    }

    #[tokio::test]
    async fn call_llm_with_retry_exhausts_all_attempts() {
        // Two context length errors, max_attempts=2 — second attempt has no guard,
        // so it returns the error directly.
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]).with_errors(vec![
            LlmError::ContextLengthExceeded,
            LlmError::ContextLengthExceeded,
        ]));
        let mut agent = agent_with_provider(provider);
        agent.context_manager.budget = Some(zeph_core_budget_for_test());
        let result: Result<Option<String>, _> = agent.call_llm_with_retry(2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_context_length_error());
    }
}

mod retry_integration {
    use crate::agent::agent_tests::*;
    use zeph_llm::LlmError;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessageMetadata, Role, ToolDefinition};

    fn agent_with_provider(provider: AnyProvider) -> crate::agent::Agent<MockChannel> {
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent
    }

    fn budget_for_test() -> crate::context::ContextBudget {
        crate::context::ContextBudget::new(200_000, 0.20)
    }

    fn no_tools() -> Vec<ToolDefinition> {
        vec![]
    }

    #[tokio::test]
    async fn call_chat_with_tools_retry_succeeds_on_first_attempt() {
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
        let mut agent = agent_with_provider(provider);
        let result = agent
            .call_chat_with_tools_retry(&no_tools(), 2)
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn call_chat_with_tools_retry_recovers_after_context_error() {
        // First call returns ContextLengthExceeded, second succeeds.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["recovered".into()])
                .with_errors(vec![LlmError::ContextLengthExceeded]),
        );
        let mut agent = agent_with_provider(provider);
        agent.context_manager.budget = Some(budget_for_test());
        let result = agent
            .call_chat_with_tools_retry(&no_tools(), 2)
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn call_chat_with_tools_retry_exhausts_all_attempts() {
        // Both attempts return ContextLengthExceeded — final error propagates.
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]).with_errors(vec![
            LlmError::ContextLengthExceeded,
            LlmError::ContextLengthExceeded,
        ]));
        let mut agent = agent_with_provider(provider);
        agent.context_manager.budget = Some(budget_for_test());
        let result: Result<Option<_>, _> = agent.call_chat_with_tools_retry(&no_tools(), 2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_context_length_error());
    }
}

// Regression tests for issue #1003: tool output must reach all channel types
// regardless of whether the tool streamed its output.
#[tokio::test]
async fn handle_tool_result_sends_output_when_streamed_true() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_tools::executor::ToolOutput;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "streamed content".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: true,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    };
    agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("bash")),
        "send_tool_output must be called even when streamed=true; got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_tool_result_fenced_emits_tool_start_then_output_via_loopback() {
    use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
    use crate::channel::{LoopbackChannel, LoopbackEvent};
    use zeph_tools::executor::ToolOutput;

    let (loopback, mut handle) = LoopbackChannel::pair(32);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, loopback, registry, None, 5, executor);

    let output = ToolOutput {
        tool_name: "grep".into(),
        summary: "match found".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    };
    agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();

    drop(agent);

    let mut events = Vec::new();
    while let Ok(ev) = handle.output_rx.try_recv() {
        events.push(ev);
    }

    let tool_start_pos = events.iter().position(|e| {
        matches!(e, LoopbackEvent::ToolStart(data)
            if data.tool_name == "grep" && !data.tool_call_id.is_empty())
    });
    let tool_output_pos = events.iter().position(|e| {
        matches!(e, LoopbackEvent::ToolOutput(data)
            if data.tool_name == "grep" && !data.tool_call_id.is_empty())
    });

    assert!(
        tool_start_pos.is_some(),
        "LoopbackEvent::ToolStart with non-empty tool_call_id must be emitted; events: {events:?}"
    );
    assert!(
        tool_output_pos.is_some(),
        "LoopbackEvent::ToolOutput with non-empty tool_call_id must be emitted; events: {events:?}"
    );
    assert!(
        tool_start_pos < tool_output_pos,
        "ToolStart must precede ToolOutput; start={tool_start_pos:?} output={tool_output_pos:?}"
    );

    // Verify both events share the same tool_call_id.
    let start_id = events.iter().find_map(|e| {
        if let LoopbackEvent::ToolStart(data) = e {
            Some(data.tool_call_id.clone())
        } else {
            None
        }
    });
    let output_id = events.iter().find_map(|e| {
        if let LoopbackEvent::ToolOutput(data) = e {
            Some(data.tool_call_id.clone())
        } else {
            None
        }
    });
    assert_eq!(
        start_id, output_id,
        "ToolStart and ToolOutput must share the same tool_call_id"
    );
}

#[tokio::test]
async fn handle_tool_result_locations_propagated_to_loopback_event() {
    use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
    use crate::channel::{LoopbackChannel, LoopbackEvent};
    use zeph_tools::executor::ToolOutput;

    let (loopback, mut handle) = LoopbackChannel::pair(32);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, loopback, registry, None, 5, executor);

    let output = ToolOutput {
        tool_name: "read_file".into(),
        summary: "file content".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: Some(vec!["/src/main.rs".to_owned()]),
        raw_response: None,
        claim_source: None,
    };
    agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    drop(agent);

    let mut events = Vec::new();
    while let Ok(ev) = handle.output_rx.try_recv() {
        events.push(ev);
    }

    let locations = events.iter().find_map(|e| {
        if let LoopbackEvent::ToolOutput(data) = e {
            data.locations.clone()
        } else {
            None
        }
    });
    assert_eq!(
        locations,
        Some(vec!["/src/main.rs".to_owned()]),
        "locations from ToolOutput must be forwarded to LoopbackEvent::ToolOutput"
    );
}

// Regression test for #1033: send_tool_output must receive raw body, not markdown-wrapped text.
// Before the fix, `format_tool_output` output (with fenced code block) was passed to
// `send_tool_output`, which caused newlines inside the output to be lost in ACP consumers
// that read `terminal_output.data` or `raw_output` as plain text.
#[tokio::test]
async fn handle_tool_result_display_is_raw_body_not_markdown_wrapped() {
    use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
    use crate::channel::{LoopbackChannel, LoopbackEvent};
    use zeph_tools::executor::ToolOutput;

    let (loopback, mut handle) = LoopbackChannel::pair(32);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, loopback, registry, None, 5, executor);

    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "line1\nline2\nline3".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    };
    agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    drop(agent);

    let mut events = Vec::new();
    while let Ok(ev) = handle.output_rx.try_recv() {
        events.push(ev);
    }

    let display = events.iter().find_map(|e| {
        if let LoopbackEvent::ToolOutput(data) = e {
            Some(data.display.clone())
        } else {
            None
        }
    });

    let display = display.expect("LoopbackEvent::ToolOutput must be emitted");
    // Raw body must be passed — no markdown fence markers.
    assert!(
        !display.contains("```"),
        "display must not contain markdown fences; got: {display:?}"
    );
    assert!(
        !display.contains("[tool output:"),
        "display must not contain markdown header; got: {display:?}"
    );
    // Newlines from the original output must be preserved.
    assert!(
        display.contains('\n'),
        "display must preserve newlines from raw body; got: {display:?}"
    );
    assert!(
        display.contains("line1") && display.contains("line2") && display.contains("line3"),
        "display must contain all lines from raw body; got: {display:?}"
    );
}

// Validate AnomalyDetector wiring: record_anomaly_outcome paths produce correct severity.
#[test]
fn anomaly_detector_15_of_20_errors_produces_critical() {
    let mut det = zeph_tools::AnomalyDetector::new(20, 0.5, 0.7);
    for _ in 0..5 {
        det.record_success();
    }
    for _ in 0..15 {
        det.record_error();
    }
    let anomaly = det.check().expect("expected anomaly");
    assert_eq!(anomaly.severity, zeph_tools::AnomalySeverity::Critical);
}

#[test]
fn anomaly_detector_5_of_20_errors_no_critical_alert() {
    let mut det = zeph_tools::AnomalyDetector::new(20, 0.5, 0.7);
    for _ in 0..15 {
        det.record_success();
    }
    for _ in 0..5 {
        det.record_error();
    }
    let result = det.check();
    assert!(
        result.is_none(),
        "5/20 errors must not trigger any alert, got: {result:?}"
    );
}
