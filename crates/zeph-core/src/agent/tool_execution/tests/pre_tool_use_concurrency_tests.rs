// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6259: `build_tier_call_futures` must fire `PreToolUse` hooks for every tool
//! call in a tier concurrently (Phase 1), not serially, before running the sequential
//! per-index gate-check loop (Phase 2). Mirrors `apply_tier_results_tests.rs`'s coverage of
//! the already-fixed `PostToolUse`/`RuntimeLayer::after_tool` twin (#6128).

use std::time::{Duration, Instant};

use zeph_config::{HookAction, HookDef, HookMatcher};
use zeph_llm::provider::{Message, MessagePart, Role, ToolUseRequest};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn make_tool_use_request(id: &str, name: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

fn sleep_hook(secs: f64) -> HookDef {
    HookDef {
        action: HookAction::Command {
            command: format!("sleep {secs}"),
        },
        timeout_secs: 5,
        fail_closed: false,
        r#if: None,
    }
}

/// N tool calls land in a single tier, each matching a `PreToolUse` hook that sleeps.
/// Serial hook dispatch (the pre-#6259 behavior) would take N * delay; concurrent dispatch
/// (Phase 1, bounded by the tier semaphore) should stay close to a single delay regardless
/// of N.
#[tokio::test]
async fn pre_tool_use_hooks_fire_concurrently_across_tier_indices() {
    let n = 4;
    let delay_secs = 0.06;
    let delay = Duration::from_millis(60);

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new((0..n).map(|_| Ok(None)).collect());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = n;
    agent.services.session.hooks_config.pre_tool_use = vec![HookMatcher {
        matcher: "noop".to_owned(),
        hooks: vec![sleep_hook(delay_secs)],
    }];
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    let tool_calls: Vec<ToolUseRequest> = (0..n)
        .map(|i| make_tool_use_request(&format!("id-{i}"), "noop"))
        .collect();

    let start = Instant::now();
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < delay * u32::try_from(n).unwrap(),
        "PreToolUse hooks appear to have fired serially: took {elapsed:?} for {n} x {delay:?}"
    );

    // Every call must still have proceeded to execution (hook is fail_open and succeeds).
    let tool_result_count = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_result_count, n,
        "every tool call must get a persisted ToolResult after its PreToolUse hook fires"
    );
}

/// Order invariant: a `fail_closed` `PreToolUse` hook block on one tier index must not affect
/// sibling indices in the same tier. Regression guard for the Phase 1 / Phase 2 split — Phase
/// 1 collects all blocked indices into a `HashMap<usize, String>` up front, and Phase 2 must
/// only consult the entry for its own idx.
#[tokio::test]
async fn pre_tool_use_hook_block_on_one_index_does_not_affect_siblings() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    // Only one call ("read") will actually reach the executor; "shell" is blocked before
    // dispatch by its fail_closed PreToolUse hook.
    let executor = MockToolExecutor::new(vec![Ok(None)]);
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = 2;
    agent.services.session.hooks_config.pre_tool_use = vec![HookMatcher {
        matcher: "shell".to_owned(),
        hooks: vec![HookDef {
            action: HookAction::Command {
                command: "exit 1".to_owned(),
            },
            timeout_secs: 5,
            fail_closed: true,
            r#if: None,
        }],
    }];
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    let tool_calls = vec![
        make_tool_use_request("id-shell", "shell"),
        make_tool_use_request("id-read", "read"),
    ];

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert_eq!(
        agent.tool_orchestrator.hook_block_count, 1,
        "exactly one call (shell) must be blocked by its own fail_closed hook"
    );

    let tool_results: Vec<(&str, &str)> = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| {
            if let MessagePart::ToolResult {
                tool_use_id,
                content,
                ..
            } = p
            {
                Some((tool_use_id.as_str(), content.as_str()))
            } else {
                None
            }
        })
        .collect();

    let shell_result = tool_results
        .iter()
        .find(|(id, _)| *id == "id-shell")
        .expect("shell ToolResult must be present");
    assert!(
        shell_result.1.contains("[blocked]"),
        "shell call must be blocked by its own hook: {shell_result:?}"
    );

    let read_result = tool_results
        .iter()
        .find(|(id, _)| *id == "id-read")
        .expect("read ToolResult must be present");
    assert!(
        !read_result.1.contains("[blocked]"),
        "read call has no matching hook and must NOT be blocked by shell's hook: {read_result:?}"
    );
}
