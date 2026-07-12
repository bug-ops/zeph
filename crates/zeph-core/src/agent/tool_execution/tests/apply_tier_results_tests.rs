// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6128: `apply_tier_results` must run `RuntimeLayer::after_tool` and `PostToolUse`
//! hook firing concurrently across a tier's tool-result indices, not serially.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zeph_llm::provider::ToolUseRequest;
use zeph_tools::ToolError;
use zeph_tools::executor::{ToolCall, ToolOutput};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::runtime_layer::{LayerContext, RuntimeLayer};

/// `RuntimeLayer` whose `after_tool` sleeps and records every `tool_call_id` it observed, so
/// tests can assert both concurrency (elapsed time) and per-index correctness (every index seen
/// exactly once).
struct DelayRecordingLayer {
    delay: Duration,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RuntimeLayer for DelayRecordingLayer {
    fn after_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        call: &'a ToolCall,
        _result: &'a Result<Option<ToolOutput>, ToolError>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            self.seen.lock().unwrap().push(call.tool_call_id.clone());
        })
    }
}

fn make_tool_use_request(id: &str, name: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

/// N tool results land in a single tier, each with a `RuntimeLayer::after_tool` that sleeps.
/// Serial processing (the pre-#6128 behavior) would take N * delay; concurrent processing
/// should stay close to a single delay regardless of N.
#[tokio::test]
async fn after_tool_hooks_run_concurrently_across_tier_indices() {
    let n = 5;
    let delay = Duration::from_millis(60);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new((0..n).map(|_| Ok(None)).collect());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = n;
    agent
        .runtime
        .config
        .layers
        .push(Arc::new(DelayRecordingLayer {
            delay,
            seen: Arc::clone(&seen),
        }));

    let tool_calls: Vec<ToolUseRequest> = (0..n)
        .map(|i| make_tool_use_request(&format!("id-{i}"), "noop"))
        .collect();

    let start = Instant::now();
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        n,
        "after_tool must run exactly once for every tool result in the tier"
    );
    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(
        unique.len(),
        n,
        "each tool result's after_tool call must carry its own tool_call_id, not a shared one"
    );
    assert!(
        elapsed < delay * u32::try_from(n).unwrap(),
        "after_tool hooks appear to have run serially: took {elapsed:?} for {n} x {delay:?}"
    );
}

/// A tier with no `RuntimeLayer`s and no `PostToolUse` hooks configured must not pay any
/// concurrency-machinery overhead — regression guard for the early-return path added in #6128.
#[tokio::test]
async fn apply_tier_results_no_layers_no_hooks_still_writes_all_results() {
    let n = 3;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new((0..n).map(|_| Ok(None)).collect());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tool_calls: Vec<ToolUseRequest> = (0..n)
        .map(|i| make_tool_use_request(&format!("id-{i}"), "noop"))
        .collect();

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    let tool_result_count = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, zeph_llm::provider::MessagePart::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_result_count, n,
        "every tool call must still get a persisted ToolResult when no layers/hooks are configured"
    );
}
