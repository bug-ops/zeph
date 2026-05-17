// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for `PreToolUse` hook `fail_closed` blocking and `hook_block_cap` enforcement (#3995).

use zeph_config::{HookAction, HookDef, HookMatcher};
use zeph_llm::provider::{Message, MessagePart, Role, ToolUseRequest};

use crate::agent::Agent;
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

fn fail_closed_hook() -> HookDef {
    HookDef {
        action: HookAction::Command {
            command: "exit 1".to_owned(),
        },
        timeout_secs: 5,
        fail_closed: true,
    }
}

fn fail_open_hook() -> HookDef {
    HookDef {
        action: HookAction::Command {
            command: "exit 1".to_owned(),
        },
        timeout_secs: 5,
        fail_closed: false,
    }
}

fn make_agent_with_pre_hook(hook: HookDef, cap: usize) -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent.services.session.hooks_config.pre_tool_use = vec![HookMatcher {
        matcher: "shell".to_owned(),
        hooks: vec![hook],
    }];
    agent.tool_orchestrator.hook_block_cap = cap;
    agent
}

/// Fix 1: `fail_closed` `PreToolUse` hook that errors must block tool execution
/// and increment `hook_block_count`.
#[tokio::test]
async fn fail_closed_hook_blocks_tool_and_increments_counter() {
    let mut agent = make_agent_with_pre_hook(fail_closed_hook(), 8);

    let tool_calls = vec![make_tool_use_request("id-1", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // Counter must be incremented exactly once (one tool blocked).
    assert_eq!(
        agent.tool_orchestrator.hook_block_count, 1,
        "hook_block_count must be 1 after one fail_closed block"
    );

    // The ToolResult must contain "[blocked]" indicating the tool was NOT executed.
    let blocked = agent.msg.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let MessagePart::ToolResult { content, .. } = p {
                content.contains("[blocked]")
            } else {
                false
            }
        })
    });
    assert!(
        blocked,
        "ToolResult must contain '[blocked]' when fail_closed hook errors"
    );
}

/// Regression guard: `fail_open` hook must NOT block the tool despite erroring.
#[tokio::test]
async fn fail_open_hook_does_not_block_tool() {
    let mut agent = make_agent_with_pre_hook(fail_open_hook(), 8);

    let tool_calls = vec![make_tool_use_request("id-2", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // Counter must remain 0 (fail_open hook errors are logged but do not block).
    assert_eq!(
        agent.tool_orchestrator.hook_block_count, 0,
        "hook_block_count must remain 0 for fail_open hook"
    );
}

/// Fix 2: when `hook_block_count` reaches `hook_block_cap`, the turn must end
/// and a warning must be sent to the channel.
#[tokio::test]
async fn hook_block_cap_ends_turn_and_sends_warning() {
    // cap = 1 so that a single blocked tool triggers the cap.
    let mut agent = make_agent_with_pre_hook(fail_closed_hook(), 1);

    let tool_calls = vec![make_tool_use_request("id-3", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    let cap_warning = sent
        .iter()
        .any(|m| m.contains("Stopping:") && m.contains("hook"));
    assert!(
        cap_warning,
        "channel must receive cap-reached warning; got: {sent:?}"
    );
}

/// `hook_block_cap = 0` means no cap — turn should NOT end due to hook blocks alone.
#[tokio::test]
async fn hook_block_cap_zero_means_no_cap() {
    let mut agent = make_agent_with_pre_hook(fail_closed_hook(), 0);

    let tool_calls = vec![make_tool_use_request("id-4", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    let cap_warning = sent
        .iter()
        .any(|m| m.contains("Stopping:") && m.contains("hook"));
    assert!(
        !cap_warning,
        "cap=0 must not trigger cap-reached warning; got: {sent:?}"
    );
}
