// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6128: `apply_tier_results` must run `RuntimeLayer::after_tool` and `PostToolUse`
//! hook firing concurrently across a tier's tool-result indices, not serially.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeph_llm::provider::ToolUseRequest;
use zeph_tools::ToolError;
use zeph_tools::executor::{ToolCall, ToolOutput};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::runtime_layer::{LayerContext, RuntimeLayer};

/// `RuntimeLayer` whose `after_tool` sleeps and records every `tool_call_id` it observed, so
/// tests can assert both concurrency (structurally, via a max-in-flight counter — see #6679)
/// and per-index correctness (every index seen exactly once).
struct DelayRecordingLayer {
    delay: Duration,
    seen: Arc<Mutex<Vec<String>>>,
    /// Number of `after_tool` calls currently inside their sleep.
    in_flight: AtomicUsize,
    /// High-water mark of `in_flight`, i.e. the largest number of `after_tool` calls ever
    /// observed running simultaneously.
    max_in_flight: AtomicUsize,
}

impl RuntimeLayer for DelayRecordingLayer {
    fn after_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        call: &'a ToolCall,
        _result: &'a Result<Option<ToolOutput>, ToolError>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
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
/// Concurrency is proven structurally via a max-in-flight counter (#6679) rather than by
/// racing wall-clock elapsed time against `N * delay`: serial processing (the pre-#6128
/// behavior) could never observe more than 1 hook in flight at once, while concurrent
/// processing must observe all N simultaneously in flight at some point.
#[tokio::test]
async fn after_tool_hooks_run_concurrently_across_tier_indices() {
    let n = 5;
    let delay = Duration::from_millis(150);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let layer = Arc::new(DelayRecordingLayer {
        delay,
        seen: Arc::clone(&seen),
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
    });

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new((0..n).map(|_| Ok(None)).collect());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = n;
    let layer_dyn: Arc<dyn RuntimeLayer> = layer.clone();
    agent.runtime.config.layers.push(layer_dyn);

    let tool_calls: Vec<ToolUseRequest> = (0..n)
        .map(|i| make_tool_use_request(&format!("id-{i}"), "noop"))
        .collect();

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

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
    // This proof assumes all n `after_tool` futures are polled within a single `join_all` pass
    // on one task, with the semaphore permit count == n (#6679 review D2); revisit if the
    // dispatch mechanism moves to a per-index `tokio::spawn`, or if the permit count is ever
    // clamped below n (e.g. by available CPU count).
    assert_eq!(
        layer.max_in_flight.load(Ordering::SeqCst),
        n,
        "after_tool hooks must all be in flight simultaneously at some point — serial \
         processing could never exceed 1 concurrent hook"
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
