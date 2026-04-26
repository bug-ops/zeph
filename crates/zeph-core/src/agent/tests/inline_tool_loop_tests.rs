// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "scheduler")]

use std::sync::Mutex;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, create_test_registry};

/// A `ToolExecutor` that responds to `execute_tool_call` with a fixed output sequence.
struct CallableToolExecutor {
    outputs: Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>,
}

impl CallableToolExecutor {
    fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
        Self {
            outputs: Mutex::new(outputs),
        }
    }

    fn fixed_output(summary: &str) -> Self {
        Self::new(vec![Ok(Some(ToolOutput {
            tool_name: "test_tool".into(),
            summary: summary.to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))])
    }

    fn failing() -> Self {
        Self::new(vec![Err(ToolError::InvalidParams {
            message: "tool failed".into(),
        })])
    }
}

impl ToolExecutor for CallableToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, _call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            Ok(None)
        } else {
            outputs.remove(0)
        }
    }
}

fn tool_use_response(tool_id: &str, tool_name: &str) -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: vec![ToolUseRequest {
            id: tool_id.to_owned(),
            name: tool_name.into(),
            input: serde_json::json!({"arg": "val"}),
        }],
        thinking_blocks: vec![],
    }
}

#[tokio::test]
async fn text_only_response_returns_immediately() {
    let (mock, _counter) =
        MockProvider::default().with_tool_use(vec![ChatResponse::Text("the answer".into())]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = CallableToolExecutor::new(vec![]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run_inline_tool_loop("what is 2+2?", 10).await;

    assert_eq!(result.unwrap(), "the answer");
}

#[tokio::test]
async fn single_tool_iteration_returns_final_text() {
    let (mock, counter) = MockProvider::default().with_tool_use(vec![
        tool_use_response("call-1", "test_tool"),
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = CallableToolExecutor::fixed_output("tool result");

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run_inline_tool_loop("run a tool", 10).await;

    assert_eq!(result.unwrap(), "done");
    assert_eq!(*counter.lock().unwrap(), 2);
}

#[tokio::test]
async fn loop_terminates_at_max_iterations() {
    // Provider always returns ToolUse — loop must stop after max_iterations.
    let responses: Vec<ChatResponse> = (0..25)
        .map(|i| tool_use_response(&format!("call-{i}"), "test_tool"))
        .collect();
    let (mock, counter) = MockProvider::default().with_tool_use(responses);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = CallableToolExecutor::fixed_output("ok");

    let max_iter = 5usize;
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run_inline_tool_loop("loop forever", max_iter).await;

    // Must return Ok (not panic or hang) and have called the provider exactly max_iter times.
    assert!(result.is_ok());
    assert_eq!(*counter.lock().unwrap(), u32::try_from(max_iter).unwrap());
}

#[tokio::test]
async fn tool_error_produces_is_error_result_and_loop_continues() {
    // First call: ToolUse with a failing executor → ToolResult with is_error=true.
    // Second call: Text → loop ends.
    // We verify the loop continues (doesn't abort) and returns the final text.
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_response("call-err", "test_tool"),
        ChatResponse::Text("recovered".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = CallableToolExecutor::failing();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run_inline_tool_loop("trigger error", 10).await;

    assert_eq!(result.unwrap(), "recovered");
}

#[tokio::test]
async fn multiple_tool_iterations_before_text() {
    // Two ToolUse rounds, then Text. Verifies the loop handles chained tool calls.
    let (mock, counter) = MockProvider::default().with_tool_use(vec![
        tool_use_response("call-1", "test_tool"),
        tool_use_response("call-2", "test_tool"),
        ChatResponse::Text("all done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    // Need two successful outputs for the two tool calls.
    let executor = CallableToolExecutor::new(vec![
        Ok(Some(ToolOutput {
            tool_name: "test_tool".into(),
            summary: "result-1".into(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        })),
        Ok(Some(ToolOutput {
            tool_name: "test_tool".into(),
            summary: "result-2".into(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        })),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent
        .run_inline_tool_loop("two tools then answer", 10)
        .await;

    assert_eq!(result.unwrap(), "all done");
    assert_eq!(*counter.lock().unwrap(), 3);
}

#[tokio::test]
async fn provider_error_is_propagated() {
    // MockProvider::failing() makes chat_with_tools return Err via the fallback chat() path.
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::failing());
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = CallableToolExecutor::new(vec![]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run_inline_tool_loop("this will fail", 10).await;

    assert!(result.is_err());
}

// Regression test for issue #2542: elicitation deadlock in run_inline_tool_loop.
//
// The real deadlock scenario: MCP tool sends an elicitation event and then blocks
// waiting for the agent to respond via response_tx. Meanwhile execute_tool_call_erased
// also blocks waiting for the MCP tool — neither side makes progress.
//
// The fix: select! concurrently drains elicitation_rx while awaiting the tool result.
//
// Test design: BlockingElicitingExecutor sends an elicitation event then blocks on
// `unblock_rx` (a oneshot whose sender is never signalled — it stays pending until
// the future is cancelled). When select! picks the elicitation branch it cancels the
// tool future, dropping `unblock_rx`. On the next invocation `unblock_rx` is None so
// the executor returns immediately. This guarantees select! MUST pick the elicitation
// branch on the first iteration (tool is the only blocking party). If the fix were
// absent, the test would deadlock and time out.
#[tokio::test]
async fn elicitation_event_during_tool_execution_is_handled() {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};
    use zeph_mcp::ElicitationEvent;

    struct BlockingElicitingExecutor {
        elic_tx: mpsc::Sender<ElicitationEvent>,
        // Holds the oneshot rx that the executor awaits on the first call.
        // Dropped (None) on re-invocation after select! cancels the first future.
        unblock_rx: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>>,
        sent: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolExecutor for BlockingElicitingExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            if !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let (response_tx, _response_rx) = oneshot::channel();
                let event = ElicitationEvent {
                    server_id: "test-server".to_owned(),
                    request: rmcp::model::CreateElicitationRequestParams::FormElicitationParams {
                        meta: None,
                        message: "please fill in".to_owned(),
                        requested_schema: rmcp::model::ElicitationSchema::new(
                            std::collections::BTreeMap::new(),
                        ),
                    },
                    response_tx,
                };
                let _ = self.elic_tx.send(event).await;
                // Block until select! cancels this future (simulates the MCP server
                // waiting for a response). Cancellation drops unblock_rx, causing
                // this await to resolve with Err — but the future is already dropped
                // by then. On re-invocation unblock_rx is None, so we skip blocking.
                let rx = self.unblock_rx.lock().unwrap().take();
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
            }
            Ok(Some(ToolOutput {
                tool_name: "elicit_tool".into(),
                summary: "result".into(),
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
    }

    let (elic_tx, elic_rx) = mpsc::channel::<ElicitationEvent>(4);
    // Keep _unblock_tx alive for the duration of the test so that unblock_rx.await
    // truly blocks (channel not closed) until the future holding it is cancelled.
    let (_unblock_tx, unblock_rx) = oneshot::channel::<()>();

    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_response("call-elic", "elicit_tool"),
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = BlockingElicitingExecutor {
        elic_tx,
        unblock_rx: Arc::new(std::sync::Mutex::new(Some(unblock_rx))),
        sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_mcp_elicitation_rx(elic_rx);

    // A 5-second timeout turns a deadlock into a clear test failure instead of a hang.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_inline_tool_loop("trigger elicitation", 10),
    )
    .await
    .expect("run_inline_tool_loop timed out — elicitation deadlock not fixed")
    .unwrap();

    assert_eq!(result, "done");
}
