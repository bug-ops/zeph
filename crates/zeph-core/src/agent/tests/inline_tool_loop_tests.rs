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
            ..Default::default()
        }))])
    }

    fn failing() -> Self {
        Self::new(vec![Err(ToolError::InvalidParams {
            message: "tool failed".into(),
        })])
    }
}

impl ToolExecutor for CallableToolExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn execute_tool_call(&self, _call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            Ok(None)
        } else {
            outputs.remove(0)
        }
    }

    zeph_tools::tool_executor_no_inner_defaults!();
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

    assert_eq!(result.unwrap().text, "the answer");
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

    assert_eq!(result.unwrap().text, "done");
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

    assert_eq!(result.unwrap().text, "recovered");
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
            ..Default::default()
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
            ..Default::default()
        })),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent
        .run_inline_tool_loop("two tools then answer", 10)
        .await
        .unwrap();

    assert_eq!(result.text, "all done");
    assert_eq!(*counter.lock().unwrap(), 3);

    // AC-8 (spec 009 § Verifier Tool-Call Grounding): the in-loop-collected tool_trace must
    // contain both tool calls in order, not just the narrated text.
    assert_eq!(result.tool_trace.len(), 2);
    assert!(result.tool_trace.iter().all(|t| t.tool == "test_tool"));
    assert!(result.tool_trace.iter().all(|t| t.ok));
    assert!(
        result
            .tool_trace
            .iter()
            .all(|t| t.args_summary.as_deref() == Some("val"))
    );
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

// Regression test for #6030 S1 (critic finding): `handle_run_inline_action` wraps
// `self.tool_executor` with `NetworkDenyToolExecutor` for the duration of a single inline
// turn when the task carries `NetworkScope::Deny`, since `RunInline` tasks share the
// parent agent's own tool loop (no per-spawn executor to wrap, unlike spawned sub-agents).
// This test exercises that exact mechanism directly against `run_inline_tool_loop` — the
// same call `handle_run_inline_action` awaits — proving the `fetch` tool call never
// reaches the inner executor once wrapped.
#[tokio::test]
async fn network_deny_wrapped_executor_blocks_fetch_before_reaching_inner() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FlaggingExecutor {
        called: Arc<AtomicBool>,
    }

    impl ToolExecutor for FlaggingExecutor {
        fn execute(
            &self,
            _response: &str,
        ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
        {
            std::future::ready(Ok(None))
        }

        #[allow(clippy::unused_async_trait_impl)]
        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(Some(ToolOutput {
                tool_name: "fetch".into(),
                summary: "should not be reached".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        }

        zeph_tools::tool_executor_no_inner_defaults!();
    }

    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_response("call-1", "fetch"),
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let called = Arc::new(AtomicBool::new(false));
    let executor = FlaggingExecutor {
        called: called.clone(),
    };

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Simulate the swap `handle_run_inline_action` performs when `network_denied_for_task`
    // returns `true` for the dispatched task.
    agent.tool_executor = Arc::new(zeph_subagent::NetworkDenyToolExecutor::new(
        agent.tool_executor.clone(),
    ));

    let result = agent.run_inline_tool_loop("fetch a url", 10).await;

    assert_eq!(result.unwrap().text, "done");
    assert!(
        !called.load(Ordering::SeqCst),
        "fetch tool call must be blocked before reaching the inner executor"
    );
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
        fn execute(
            &self,
            _response: &str,
        ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
        {
            std::future::ready(Ok(None))
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            if !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let (response_tx, _response_rx) = oneshot::channel();
                let event = ElicitationEvent {
                    server_id: "test-server".to_owned(),
                    request: rmcp::model::ElicitRequestParams::FormElicitationParams {
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
                ..Default::default()
            }))
        }

        zeph_tools::tool_executor_no_inner_defaults!();
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

    assert_eq!(result.text, "done");
}

// spec-075 (#6243) Phase 5: RunInline per-task `run_timeout_secs` enforcement via
// `handle_run_inline_action`'s third `tokio::select!` branch. These exercise the full
// `Agent::run_scheduler_loop` seam (not just `run_inline_tool_loop` in isolation), since the
// timeout branch lives in the scheduler-dispatch wrapper, not the inner tool loop.
mod run_inline_timeout {
    use std::time::Duration;

    use zeph_orchestration::{
        DagScheduler, GraphStatus, RuleBasedRouter, TaskGraph, TaskNode, TaskStatus, TimeoutPolicy,
    };

    use super::*;

    struct SlowToolExecutor {
        delay: Duration,
    }

    impl ToolExecutor for SlowToolExecutor {
        fn execute(
            &self,
            _response: &str,
        ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
        {
            std::future::ready(Ok(None))
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            tokio::time::sleep(self.delay).await;
            Ok(Some(ToolOutput {
                tool_name: "test_tool".into(),
                summary: "slow result".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        }

        zeph_tools::tool_executor_no_inner_defaults!();
    }

    /// T5.3: a `RunInline` task with a short `run_timeout_secs` override and a tool loop that
    /// runs longer than the override (but well under the long global default) — the timeout
    /// branch must fire and fail the graph (default `Abort` strategy).
    #[tokio::test]
    async fn short_override_fires_before_slow_tool_loop_completes() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-1", "test_tool"),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = SlowToolExecutor {
            delay: Duration::from_secs(3),
        };

        let mut graph = TaskGraph::new("slow run-inline task");
        let mut node = TaskNode::new(0, "slow task", "run something slow");
        node.timeout = Some(TimeoutPolicy {
            run_timeout_secs: Some(1),
            idle_timeout_secs: None,
        });
        graph.tasks.push(node);

        let config = zeph_config::OrchestrationConfig {
            task_timeout_secs: 300, // long global default — proves the override (not it) fired
            ..zeph_config::OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 1, token),
        )
        .await
        .expect("run_scheduler_loop must not hang past the 1s override")
        .unwrap();

        assert_eq!(
            status,
            GraphStatus::Failed,
            "timed-out RunInline task with default Abort strategy fails the graph"
        );
        assert_eq!(scheduler.graph().tasks[0].status, TaskStatus::Failed);
    }

    /// T5.4 regression: a `RunInline` task with no override and a fast-completing tool loop
    /// completes normally — the new timeout branch must never fire when unused.
    #[tokio::test]
    async fn no_override_fast_completion_is_unaffected() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-1", "test_tool"),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::fixed_output("fast result");

        let mut graph = TaskGraph::new("fast run-inline task");
        let node = TaskNode::new(0, "fast task", "run something fast");
        graph.tasks.push(node);

        let config = zeph_config::OrchestrationConfig::default();
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(status, GraphStatus::Completed);
        assert_eq!(scheduler.graph().tasks[0].status, TaskStatus::Completed);
    }

    /// Behavior-change regression (CHANGELOG `[Unreleased]` "BEHAVIOR CHANGE" entry): a
    /// `RunInline` task with **no** per-task `timeout` override was previously unbounded on
    /// this dispatch path (`check_timeouts()` cannot observe a task blocking the tick loop for
    /// its whole duration). It is now capped by the graph-global `task_timeout_secs` default,
    /// exactly like a spawned task. This test uses a short global default (rather than waiting
    /// out the real 300s default) to prove the cap applies even with zero per-task
    /// configuration.
    #[tokio::test]
    async fn no_override_task_is_capped_by_global_default_previously_unbounded() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-1", "test_tool"),
            ChatResponse::Text("unused".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = SlowToolExecutor {
            delay: Duration::from_secs(3),
        };

        let mut graph = TaskGraph::new("no-override slow run-inline task");
        // No `.timeout` set on this node — relies entirely on the graph-global default.
        let node = TaskNode::new(0, "slow task, no override", "run something slow");
        graph.tasks.push(node);

        let config = zeph_config::OrchestrationConfig {
            task_timeout_secs: 1, // short global default stands in for the real 300s default
            ..zeph_config::OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 1, token),
        )
        .await
        .expect("run_scheduler_loop must not hang past the 1s global default")
        .unwrap();

        assert_eq!(
            status,
            GraphStatus::Failed,
            "a RunInline task with no override must now be capped by the global default \
             (previously this dispatch path was entirely unbounded)"
        );
        assert_eq!(scheduler.graph().tasks[0].status, TaskStatus::Failed);
    }

    /// T5.5 (cross-phase Phase 3 + Phase 5): a `RunInline` task with both `timeout` and
    /// `recovery` configured — the timeout fires, Mode-1 recovery applies (since the default
    /// strategy is `Abort`), and the dependent task unblocks and dispatches.
    #[tokio::test]
    async fn timeout_and_recovery_together_unblocks_dependent() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            // task 0: the tool_use response is consumed, but the ensuing tool call sleeps
            // past the 1s override — the select! timeout branch cancels the loop before a
            // second provider call would ever happen for this task.
            tool_use_response("call-1", "test_tool"),
            // task 1 (the dependent, unblocked by recovery): completes immediately.
            ChatResponse::Text("dependent done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = SlowToolExecutor {
            delay: Duration::from_secs(3),
        };

        let mut graph = TaskGraph::new("timeout + recovery run-inline test");
        let mut node0 = TaskNode::new(0, "slow recoverable task", "run something slow");
        node0.timeout = Some(TimeoutPolicy {
            run_timeout_secs: Some(1),
            idle_timeout_secs: None,
        });
        node0.recovery = Some(zeph_orchestration::RecoveryAction {
            state_injection: Some("recovered output".to_string()),
            route_to: None,
        });
        let mut node1 = TaskNode::new(1, "dependent task", "consume the recovered output");
        node1.depends_on = vec![zeph_orchestration::TaskId(0)];
        graph.tasks.push(node0);
        graph.tasks.push(node1);

        let config = zeph_config::OrchestrationConfig {
            task_timeout_secs: 300,
            ..zeph_config::OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 2, token),
        )
        .await
        .expect("run_scheduler_loop must not hang past the 1s override")
        .unwrap();

        assert_eq!(
            status,
            GraphStatus::Completed,
            "recovery absorbs task 0's timeout; graph continues and completes via task 1"
        );
        assert_eq!(scheduler.graph().tasks[0].status, TaskStatus::Completed);
        assert_eq!(
            scheduler.graph().tasks[0]
                .result
                .as_ref()
                .unwrap()
                .agent_def
                .as_deref(),
            Some("__recovery__")
        );
        assert_eq!(scheduler.graph().tasks[1].status, TaskStatus::Completed);
    }
}
