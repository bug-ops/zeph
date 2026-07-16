// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression coverage for #6330: no test previously exercised a real turn that dispatches
//! tool calls and asserts `LifecycleState::turn_tool_calls` / `turn_llm_requests` reflect the
//! actual per-turn counts, rather than the pre-#6328 hardcoded `0`.
//!
//! These tests assert on the `LifecycleState` fields directly rather than on a constructed
//! `TurnSummary`. `Agent::maybe_fire_completion_notification` (see `crate::agent::mod`, the
//! single construction site at line ~1396) builds `TurnSummary` as a direct, untransformed copy
//! of `LifecycleState::turn_tool_calls` / `turn_llm_requests`, and `Agent::end_turn` never
//! touches either counter — only `Agent::begin_turn` resets them — so the values asserted here
//! are exactly what a constructed `TurnSummary` for that turn would carry.
//!
//! This is real regression coverage for the counter logic (reset site, increment site, batch
//! counting) — not an end-to-end proof of delivery to a downstream consumer. Both
//! `TurnSummary::llm_requests` and `TurnSummary::tool_calls` now have shipped consumers:
//! `llm_requests` gates `Notifier::should_fire` and is exported to `turn_complete` hooks as
//! `ZEPH_TURN_LLM_REQUESTS`; `tool_calls` feeds the notification body (non-zero counts only,
//! see `crate::notifications::build_notification_message`) and is exported as
//! `ZEPH_TURN_TOOL_CALLS` — both in `crate::notifications` / `mod.rs`'s hook-env-building code.

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_tools::executor::ToolOutput;

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};
use crate::notifications::{TurnExitStatus, TurnSummary};

fn tool_output(name: &str, summary: &str) -> ToolOutput {
    ToolOutput {
        tool_name: name.into(),
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
    }
}

/// A single-batch `ChatResponse::ToolUse` carrying `n` distinct tool calls, mirroring how the
/// LLM can request several tool invocations in one round-trip. `check_and_update_quota`
/// (`crate::agent::tool_execution::tier_loop`) counts the whole batch in a single call, so this
/// is the shape needed to catch off-by-one/batch-counting bugs.
fn tool_use_batch(n: usize) -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: (0..n)
            .map(|i| ToolUseRequest {
                id: format!("call-{i}"),
                name: format!("tool_{i}").into(),
                input: serde_json::json!({"arg": i}),
            })
            .collect(),
        thinking_blocks: vec![],
    }
}

/// Baseline (AC #1): a turn with zero tool calls must report `turn_tool_calls == 0` and exactly
/// one LLM round-trip, not a stale or hardcoded value.
#[tokio::test]
async fn text_only_turn_reports_zero_tool_calls_and_one_llm_request() {
    let (mock, _counter) =
        MockProvider::default().with_tool_use(vec![ChatResponse::Text("hello".into())]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent
        .process_user_message("no tools needed".to_owned(), vec![])
        .await
        .unwrap();

    assert_eq!(agent.runtime.lifecycle.turn_tool_calls, 0);
    assert_eq!(agent.runtime.lifecycle.turn_llm_requests, 1);
}

/// AC #2 (multiple tool calls in a single turn): a `ToolUse` response with 3 tool calls
/// dispatched in one batch, followed by the final text, must land `turn_tool_calls == 3` — not
/// `1` (would indicate the batch is miscounted as a single call) and not `0` (would indicate the
/// pre-#6328 hardcoded-zero regression). `turn_llm_requests` must be `2`: the `ToolUse`
/// round-trip plus the final `Text` round-trip.
#[tokio::test]
async fn single_turn_with_multiple_tool_calls_counts_full_batch() {
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_batch(3),
        ChatResponse::Text("all done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![
        Ok(Some(tool_output("tool_0", "result-0"))),
        Ok(Some(tool_output("tool_1", "result-1"))),
        Ok(Some(tool_output("tool_2", "result-2"))),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent
        .process_user_message("run three tools".to_owned(), vec![])
        .await
        .unwrap();

    assert_eq!(agent.runtime.lifecycle.turn_tool_calls, 3);
    assert_eq!(agent.runtime.lifecycle.turn_llm_requests, 2);
}

/// AC #3 (reset-timing regression guard): a turn that dispatches tool calls must not leak its
/// counts into the *next* turn. This directly exercises the single reset site,
/// `Agent::begin_turn` (`self.runtime.lifecycle.turn_tool_calls = 0` /
/// `self.runtime.lifecycle.turn_llm_requests = 0`) — a bug there (e.g. reset removed, or moved
/// to fire before the previous turn's `TurnSummary` is built) would surface as turn 2 observing
/// turn 1's accumulated counts instead of its own.
#[tokio::test]
async fn tool_call_counter_resets_between_turns_not_accumulated() {
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_batch(2),
        ChatResponse::Text("first turn done".into()),
        ChatResponse::Text("second turn done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![
        Ok(Some(tool_output("tool_0", "r0"))),
        Ok(Some(tool_output("tool_1", "r1"))),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .process_user_message("first: run two tools".to_owned(), vec![])
        .await
        .unwrap();
    assert_eq!(agent.runtime.lifecycle.turn_tool_calls, 2);
    assert_eq!(agent.runtime.lifecycle.turn_llm_requests, 2);

    agent
        .process_user_message("second: no tools".to_owned(), vec![])
        .await
        .unwrap();
    assert_eq!(
        agent.runtime.lifecycle.turn_tool_calls, 0,
        "turn 2 dispatched no tools; a leaked/accumulated counter would show 2 here"
    );
    assert_eq!(
        agent.runtime.lifecycle.turn_llm_requests, 1,
        "turn 2 made exactly one LLM round-trip; an accumulated counter would show 3 here"
    );
}

/// Regression coverage for #6335: `TurnSummary::tool_calls` must reach the `turn_complete`
/// hook environment as `ZEPH_TURN_TOOL_CALLS`, mirroring `ZEPH_TURN_LLM_REQUESTS`.
#[test]
fn hook_env_includes_tool_calls_count() {
    let summary = TurnSummary {
        duration_ms: 1234,
        preview: "done".to_owned(),
        tool_calls: 3,
        llm_requests: 2,
        exit_status: TurnExitStatus::Success,
    };

    let env = crate::agent::build_turn_hook_env(&summary, false);

    assert_eq!(env.get("ZEPH_TURN_TOOL_CALLS"), Some(&"3".to_owned()));
    assert_eq!(env.get("ZEPH_TURN_LLM_REQUESTS"), Some(&"2".to_owned()));
    assert_eq!(env.get("ZEPH_TURN_DURATION_MS"), Some(&"1234".to_owned()));
    assert_eq!(env.get("ZEPH_TURN_STATUS"), Some(&"success".to_owned()));
}
