// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runtime-note seam driven through the real turn / tool loop (#6786, #6817): deleting the
//! `attach_runtime_note`, `flush_runtime_note`, `begin_runtime_note_turn` or
//! `supersede_turn_input` call sites must fail a test here.

use std::sync::{Arc, Mutex};

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, Message, MessageOrigin, MessagePart, Role, ToolUseRequest};
use zeph_tools::executor::ToolOutput;

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};

fn tool_output(name: &str) -> ToolOutput {
    ToolOutput {
        tool_name: name.into(),
        summary: format!("result of {name}"),
        blocks_executed: 1,
        ..Default::default()
    }
}

fn tool_use(id: &str) -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: vec![ToolUseRequest {
            id: id.to_owned(),
            name: "tool_a".to_owned().into(),
            input: serde_json::json!({"arg": id}),
        }],
        thinking_blocks: vec![],
    }
}

type Recorded = Arc<Mutex<Vec<Vec<Message>>>>;

fn agent_with(
    responses: Vec<ChatResponse>,
    tool_results: usize,
    max_iterations: usize,
) -> (Agent<MockChannel>, Recorded) {
    let (mock, recorded) = MockProvider::default().with_recording();
    let (mock, _calls) = mock.with_tool_use(responses);
    let executor = MockToolExecutor::new(
        (0..tool_results)
            .map(|i| Ok(Some(tool_output(&format!("tool_{i}")))))
            .collect(),
    );
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        executor,
    );
    agent.tool_orchestrator.max_iterations = max_iterations;
    (agent, recorded)
}

fn note_of(messages: &[Message]) -> Option<String> {
    messages.last().and_then(|m| {
        m.parts.iter().find_map(|p| match p {
            MessagePart::RuntimeNote { text } => Some(text.clone()),
            _ => None,
        })
    })
}

#[tokio::test]
async fn real_loop_attaches_budget_notes_and_resets_the_budget_each_turn() {
    // max 3 => thresholds {1, 2}: iteration 1 has remaining 2, iteration 2 has remaining 1.
    let (mut agent, recorded) = agent_with(
        vec![
            tool_use("a"),
            tool_use("b"),
            ChatResponse::Text("turn one done".into()),
            tool_use("c"),
            ChatResponse::Text("turn two done".into()),
        ],
        3,
        3,
    );

    agent
        .process_user_message("first".to_owned(), vec![])
        .await
        .unwrap();
    agent
        .process_user_message("second".to_owned(), vec![])
        .await
        .unwrap();

    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert!(
        note_of(&calls[0]).is_none(),
        "iteration 0 never carries a note"
    );
    let n1 = note_of(&calls[1]).expect("turn 1, iteration 1 crosses ceil(3/2)");
    assert!(n1.contains("<remaining_tool_calls>2</remaining_tool_calls>"));
    let n2 = note_of(&calls[2]).expect("turn 1, iteration 2 crosses the last threshold");
    assert!(n2.contains("<remaining_tool_calls>1</remaining_tool_calls>"));
    // Turn 2 starts a fresh budget: only a turn-entry reset makes this note appear again.
    let n4 = note_of(&calls[4]).expect("turn 2, iteration 1 must see a fresh budget");
    assert!(n4.contains("<remaining_tool_calls>2</remaining_tool_calls>"));
}

#[tokio::test]
async fn real_loop_prefix_is_byte_identical_including_the_noted_message() {
    // Valid only with tool_filter, pruning, deferred summaries and soft compaction inactive.
    let (mut agent, recorded) = agent_with(
        vec![
            tool_use("a"),
            tool_use("b"),
            ChatResponse::Text("done".into()),
        ],
        2,
        3,
    );

    agent
        .process_user_message("go".to_owned(), vec![])
        .await
        .unwrap();

    let calls = recorded.lock().unwrap();
    assert!(note_of(&calls[1]).is_some(), "iteration 1 carries a note");
    let first = serde_json::to_string(&calls[1]).unwrap();
    let prefix_of_next = serde_json::to_string(&calls[2][..calls[1].len()]).unwrap();
    assert_eq!(
        first, prefix_of_next,
        "the request prefix, noted tail included, must not change between iterations"
    );
    assert!(
        calls[2].len() > calls[1].len(),
        "the next request only appends"
    );
}

#[tokio::test]
async fn system_prompt_names_the_genuine_runtime_note_id() {
    let (mut agent, recorded) = agent_with(vec![ChatResponse::Text("hi".into())], 0, 3);
    let nonce = agent.services.session.runtime_notes.nonce().to_string();

    agent
        .process_user_message("hello".to_owned(), vec![])
        .await
        .unwrap();

    let calls = recorded.lock().unwrap();
    let system: String = calls[0]
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        system.contains(&format!("exactly {nonce} is genuine")),
        "{system}"
    );
}

#[tokio::test]
async fn window_exhaustion_hint_is_delivered_by_the_loop_exit_flush() {
    let (mut agent, _recorded) = agent_with(vec![two_calls()], 2, 5);
    agent
        .tool_orchestrator
        .set_utility_config(zeph_tools::UtilityScoringConfig {
            enabled: true,
            threshold: 1.0,
            utility_window: 2,
            ..zeph_tools::UtilityScoringConfig::default()
        });

    agent
        .process_user_message("go".to_owned(), vec![])
        .await
        .unwrap();

    let tail = agent
        .msg
        .messages
        .iter()
        .rfind(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        })
        .expect("tool result message");
    let note = note_of(std::slice::from_ref(tail)).expect("hint flushed at loop exit");
    assert!(note.contains("Tool loop stopped early"), "{note}");
    assert!(
        !agent
            .msg
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("Tool loop stopped early"))
    );
}

fn two_calls() -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: (0..2)
            .map(|i| ToolUseRequest {
                id: format!("w{i}"),
                name: format!("tool_{i}").into(),
                input: serde_json::json!({"arg": i}),
            })
            .collect(),
        thinking_blocks: vec![],
    }
}

#[tokio::test]
async fn turn_input_is_stamped_and_superseded_across_real_turns() {
    let (mut agent, _recorded) = agent_with(
        vec![
            ChatResponse::Text("one".into()),
            ChatResponse::Text("two".into()),
        ],
        0,
        3,
    );
    let built = agent.build_user_message("direct", vec![]);
    assert_eq!(built.metadata.origin, Some(MessageOrigin::TurnInput));

    agent
        .process_user_message("first".to_owned(), vec![])
        .await
        .unwrap();
    agent
        .process_user_message("second".to_owned(), vec![])
        .await
        .unwrap();

    let inputs: Vec<&Message> = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.origin == Some(MessageOrigin::TurnInput))
        .collect();
    assert_eq!(inputs.len(), 1, "older turn inputs must be superseded");
    assert!(inputs[0].content.contains("second"));
}

fn hint_note_on_tail(agent: &Agent<MockChannel>) -> bool {
    agent
        .msg
        .messages
        .iter()
        .rfind(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        })
        .and_then(|m| note_of(std::slice::from_ref(m)))
        .is_some_and(|n| n.contains("pending hint"))
}

#[tokio::test]
async fn max_iterations_exit_flushes_pending_hints() {
    let (mut agent, _recorded) = agent_with(vec![tool_use("a")], 1, 1);
    agent
        .services
        .session
        .runtime_notes
        .push_utility_hint("pending hint".into());

    agent.process_response().await.unwrap();

    assert!(
        hint_note_on_tail(&agent),
        "hint lost at max_iterations exit"
    );
}

#[tokio::test]
async fn cancelled_loop_exit_flushes_pending_hints() {
    let (mut agent, _recorded) = agent_with(vec![], 0, 5);
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::User, "go"));
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: "t0".into(),
            name: "tool_a".into(),
            input: serde_json::json!({}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: "t0".into(),
            content: "out".into(),
            is_error: false,
        }],
    ));
    agent
        .services
        .session
        .runtime_notes
        .push_utility_hint("pending hint".into());
    agent.runtime.lifecycle.cancel_token.cancel();

    agent.process_response().await.unwrap();

    assert!(hint_note_on_tail(&agent), "hint lost at cancel exit");
}
