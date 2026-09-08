// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression coverage for #6765: `BudgetHint.remaining_tool_calls`, injected into the system
//! prompt by `rebuild_system_prompt` (`crate::agent::context::assembly`), used to be computed
//! from `ToolState::current_tool_iteration` — a counter that was written per tool-loop
//! iteration (`tier_loop.rs`) but never reset at turn start. A new turn therefore inherited the
//! previous turn's last iteration value and reported a shrunk budget instead of the full
//! `max_tool_calls` allowance. Since `rebuild_system_prompt` only ever runs once per turn,
//! before the tool loop starts, the fix removed the dead per-iteration counter entirely:
//! `remaining_tool_calls` is now set directly to `max_tool_calls` at that once-per-turn seam.

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_tools::executor::ToolOutput;

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};

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

/// Extracts the `<remaining_tool_calls>N</remaining_tool_calls>` value from the agent's current
/// system prompt. `rebuild_system_prompt` overwrites `messages[0].content` (the system message)
/// in place every turn, so this reads exactly what the LLM would see for the most recent turn.
fn remaining_tool_calls_in_system_prompt(agent: &Agent<MockChannel>) -> usize {
    let prompt = &agent.msg.messages[0].content;
    let tag_start = "<remaining_tool_calls>";
    let tag_end = "</remaining_tool_calls>";
    let start = prompt
        .find(tag_start)
        .expect("BudgetHint must be injected into the system prompt (max_tool_calls > 0)")
        + tag_start.len();
    let end = start
        + prompt[start..]
            .find(tag_end)
            .expect("closing tag must follow");
    prompt[start..end]
        .parse()
        .expect("remaining_tool_calls must be a valid integer")
}

/// AC (root cause, #6765): a turn that consumes tool-loop iterations must not leak a stale
/// iteration count into the *next* turn's `BudgetHint`. Turn 2's system prompt must always
/// advertise the full, fresh `max_tool_calls` budget, regardless of how many iterations turn 1
/// consumed.
#[tokio::test]
async fn budget_hint_shows_full_budget_after_prior_turn_consumed_iterations() {
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        tool_use_batch(1),
        ChatResponse::Text("first turn done".into()),
        ChatResponse::Text("second turn done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Ok(Some(tool_output("tool_0", "r0")))]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let max_tool_calls = agent.tool_orchestrator.max_iterations;

    agent
        .process_user_message("first: run one tool".to_owned(), vec![])
        .await
        .unwrap();

    agent
        .process_user_message("second: no tools".to_owned(), vec![])
        .await
        .unwrap();

    assert_eq!(
        remaining_tool_calls_in_system_prompt(&agent),
        max_tool_calls,
        "turn 2's BudgetHint must show the full budget, not max_tool_calls minus turn 1's stale \
         iteration count"
    );
}
