// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stale tool-output pruning runs once per turn, ahead of and independent from context
//! preparation, so skipping or timing out that preparation cannot leave the history unpruned.

use zeph_llm::provider::{Message, MessagePart, Role};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn tool_pair(id: &str) -> [Message; 2] {
    [
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: id.into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: "very long tool output ".repeat(40),
                is_error: false,
            }],
        ),
    ]
}

#[tokio::test]
async fn stale_tool_output_is_pruned_even_when_context_preparation_is_skipped() {
    let mut agent = crate::agent::Agent::new(
        mock_provider(vec!["ok".into()]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent.msg.messages.clear();
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::User, "earlier"));
    for i in 0..5 {
        agent.msg.messages.extend(tool_pair(&format!("t{i}")));
    }
    agent.services.memory.persistence.tool_call_cutoff = 1;
    agent.runtime.config.timeouts.no_providers_backoff_secs = 3600;
    agent.runtime.lifecycle.last_no_providers_at = Some(std::time::Instant::now());

    agent
        .process_user_message("next".to_owned(), vec![])
        .await
        .unwrap();

    let first_result = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .find_map(|p| match p {
            MessagePart::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "t0" => Some(content.clone()),
            _ => None,
        })
        .expect("oldest tool result");
    assert_eq!(first_result, "[pruned]");
}
