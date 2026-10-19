// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Frozen turn tool set, dispatch gates, and ordered thinking blocks in the native tool loop.

use std::collections::HashSet;
use std::sync::Arc;

use zeph_llm::LlmError;
use zeph_llm::ThinkingReplay;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{
    AssistantBlock, AssistantContent, ChatResponse, Message, MessageMetadata, MessageOrigin,
    MessagePart, Role, ThinkingBlock, ToolDefinition, ToolUseRequest,
};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, test_tool_def as tool_def,
};

fn output(tool: &str, summary: &str) -> zeph_tools::ToolOutput {
    zeph_tools::ToolOutput {
        tool_name: tool.into(),
        summary: summary.into(),
        blocks_executed: 1,
        ..Default::default()
    }
}

fn call(id: &str, name: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

fn user_turn(text: &str) -> Message {
    Message {
        metadata: MessageMetadata {
            origin: Some(MessageOrigin::TurnInput),
            ..MessageMetadata::default()
        },
        ..Message::from_legacy(Role::User, text)
    }
}

fn thinking_block(text: &str) -> ThinkingBlock {
    ThinkingBlock::Thinking {
        thinking: text.into(),
        signature: format!("sig-{text}"),
    }
}

fn thinking_part(text: &str) -> MessagePart {
    MessagePart::ThinkingBlock {
        thinking: text.into(),
        signature: format!("sig-{text}"),
    }
}

fn agent_with(provider: MockProvider, executor: MockToolExecutor) -> Agent<MockChannel> {
    let mut agent = Agent::new(
        AnyProvider::Mock(provider),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        6,
        executor,
    );
    agent.msg.messages.clear();
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    agent.msg.messages.push(user_turn("do the thing"));
    agent
}

fn graph_write_requires_read() -> Arc<zeph_tools::ToolDependencyGraph> {
    let deps = [(
        "write".to_owned(),
        zeph_tools::ToolDependency {
            requires: vec!["read".to_owned()],
            prefers: Vec::new(),
        },
    )]
    .into();
    Arc::new(zeph_tools::ToolDependencyGraph::new(deps))
}

fn tool_result_text(agent: &Agent<MockChannel>, tool_use_id: &str) -> String {
    agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .find_map(|p| match p {
            MessagePart::ToolResult {
                tool_use_id: id,
                content,
                ..
            } if id == tool_use_id => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {tool_use_id}"))
}

fn names(defs: &[ToolDefinition]) -> Vec<String> {
    defs.iter().map(|d| d.name.to_string()).collect()
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn tool_array_is_identical_on_every_iteration_of_a_turn() {
    let (mock, recorded) = MockProvider::with_responses(vec![]).with_tool_recording();
    let (mock, _) = mock.with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "read")]),
        ChatResponse::tool_use(None, vec![call("c2", "read")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![
        Ok(Some(output("read", "a"))),
        Ok(Some(output("read", "b"))),
    ])
    .with_definitions(vec![tool_def("read"), tool_def("write"), tool_def("grep")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());
    // `write` is held back only by its unmet `requires`; `grep` was dropped by the filter.
    agent.services.tool_state.cached_filtered_tool_ids =
        Some(["read".to_owned(), "write".to_owned()].into());

    agent.process_response().await.unwrap();

    let calls = recorded.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "one provider call per iteration");
    let first = names(&calls[0]);
    assert!(first.contains(&"read".to_owned()) && first.contains(&"write".to_owned()));
    assert!(!first.contains(&"grep".to_owned()));
    assert!(first.contains(&"compress_context".to_owned()));
    for later in &calls[1..] {
        assert_eq!(names(later), first, "tool array must not change mid-turn");
    }
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn tool_with_met_requires_is_callable_in_a_later_iteration() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "read")]),
        ChatResponse::tool_use(None, vec![call("c2", "write")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![
        Ok(Some(output("read", "read ok"))),
        Ok(Some(output("write", "write ok"))),
    ])
    .with_definitions(vec![tool_def("read"), tool_def("write")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());
    agent.services.tool_state.cached_filtered_tool_ids =
        Some(["read".to_owned(), "write".to_owned()].into());

    agent.process_response().await.unwrap();

    assert!(tool_result_text(&agent, "c2").contains("write ok"));
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn tool_with_unmet_requires_is_rejected_at_dispatch() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "write")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("write", "write ok")))])
        .with_definitions(vec![tool_def("read"), tool_def("write"), tool_def("grep")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());
    agent.services.tool_state.dependency_always_on = HashSet::from(["grep".to_owned()]);

    agent.process_response().await.unwrap();

    let text = tool_result_text(&agent, "c1");
    assert!(text.contains("[error] Tool call to"), "got: {text}");
    assert!(text.contains("requires read"), "got: {text}");
    assert!(!text.contains("write ok"), "the tool must not have run");
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn tool_outside_the_turn_set_is_rejected_at_dispatch() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "grep")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("grep", "grep ok")))])
        .with_definitions(vec![tool_def("read"), tool_def("grep")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.cached_filtered_tool_ids = Some(["read".to_owned()].into());

    agent.process_response().await.unwrap();

    let text = tool_result_text(&agent, "c1");
    assert!(text.contains("[error] Tool call to"), "got: {text}");
    assert!(text.contains("not available"), "got: {text}");
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn ordered_blocks_are_stored_in_response_order_and_not_persisted() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("read", "ok")))])
        .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor);
    let content = AssistantContent::new(vec![
        AssistantBlock::Thinking(thinking_block("first")),
        AssistantBlock::Text("plan".into()),
        AssistantBlock::Thinking(thinking_block("second")),
        AssistantBlock::ToolUse(call("c1", "read")),
    ]);

    agent.handle_native_content(content).await.unwrap();

    let asst = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant message");
    assert!(matches!(
        asst.parts.as_slice(),
        [
            MessagePart::ThinkingBlock { thinking: a, .. },
            MessagePart::Text { .. },
            MessagePart::ThinkingBlock { thinking: b, .. },
            MessagePart::ToolUse { .. },
        ] if a == "first" && b == "second"
    ));
    let persisted = MessagePart::strip_for_persistence(&asst.parts);
    assert!(matches!(
        persisted.as_slice(),
        [MessagePart::Text { .. }, MessagePart::ToolUse { .. }]
    ));
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn scrubbed_text_drops_the_thinking_that_follows_it() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("read", "ok")))])
        .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor);
    let content = AssistantContent::new(vec![
        AssistantBlock::Thinking(thinking_block("before")),
        AssistantBlock::Text("see ![x](https://evil.example/p.png?d=SECRET) here".into()),
        AssistantBlock::Thinking(thinking_block("after")),
        AssistantBlock::ToolUse(call("c1", "read")),
    ]);

    agent.handle_native_content(content).await.unwrap();

    let asst = agent
        .msg
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant message");
    let kept: Vec<&str> = asst
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::ThinkingBlock { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kept, ["before"], "thinking after a scrubbed text must go");
}

fn history_with_thinking() -> Vec<Message> {
    vec![
        Message::from_legacy(Role::System, "system"),
        user_turn("go"),
        Message::from_parts(
            Role::Assistant,
            vec![
                thinking_part("t0"),
                MessagePart::ToolUse {
                    id: "h1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "h1".into(),
                content: "r".into(),
                is_error: false,
            }],
        ),
    ]
}

fn thinking_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::ThinkingBlock { .. }))
        .count()
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn provider_dropped_thinking_is_stripped_from_history_once() {
    let response = ChatResponse::ToolUse {
        content: AssistantContent::new(vec![
            AssistantBlock::Thinking(thinking_block("fresh")),
            AssistantBlock::ToolUse(call("c1", "read")),
        ]),
        replay: ThinkingReplay::Dropped {
            first_message_index: 1,
        },
        compaction: None,
    };
    let (mock, _) = MockProvider::with_responses(vec![])
        .with_tool_use(vec![response, ChatResponse::Text("done".into())]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("read", "ok")))])
        .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor);
    agent.msg.messages = history_with_thinking();

    agent.process_response().await.unwrap();

    let history = &agent.msg.messages;
    assert!(
        !history[2]
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ThinkingBlock { .. })),
        "stale thinking must be stripped"
    );
    assert_eq!(
        thinking_count(history),
        1,
        "only the thinking of the response itself remains"
    );
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn binding_mismatch_strips_all_thinking_and_retries_once() {
    let (mock, counter) = MockProvider::with_responses(vec!["recovered".into()])
        .with_errors(vec![LlmError::ThinkingBindingMismatch])
        .with_tool_use(vec![]);
    let mut agent = agent_with(mock, MockToolExecutor::no_tools());
    agent.msg.messages = history_with_thinking();

    agent.process_response().await.unwrap();

    assert_eq!(thinking_count(&agent.msg.messages), 0);
    assert_eq!(*counter.lock().unwrap(), 2, "exactly one retry");
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn binding_mismatch_is_not_retried_twice() {
    let (mock, counter) = MockProvider::with_responses(vec![])
        .with_errors(vec![
            LlmError::ThinkingBindingMismatch,
            LlmError::ThinkingBindingMismatch,
        ])
        .with_tool_use(vec![]);
    let mut agent = agent_with(mock, MockToolExecutor::no_tools());
    agent.msg.messages = history_with_thinking();

    let result = agent.process_response().await;

    assert!(result.is_err());
    assert_eq!(*counter.lock().unwrap(), 2);
}

#[tokio::test]
async fn empty_server_compaction_summary_keeps_history() {
    let mut agent = agent_with(
        MockProvider::with_responses(vec![]),
        MockToolExecutor::no_tools(),
    );
    agent.msg.messages = history_with_thinking();
    let before = agent.msg.messages.len();

    assert!(!agent.apply_raw_server_compaction("   "));

    assert_eq!(agent.msg.messages.len(), before);
}

#[tokio::test]
async fn deferred_apply_strips_thinking_from_the_lowest_touched_index() {
    use zeph_agent_context::DeferredApplyTrigger;

    let mut agent = agent_with(
        MockProvider::with_responses(vec![]),
        MockToolExecutor::no_tools(),
    );
    let mut history = history_with_thinking();
    history[3].metadata.deferred_summary = Some("did read".into());
    history.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("later"),
            MessagePart::Text { text: "x".into() },
        ],
    ));
    agent.msg.messages = history;
    agent.services.memory.persistence.tool_call_cutoff = 1;

    agent.maybe_apply_deferred_summaries(DeferredApplyTrigger::TokenOrCount);

    assert_eq!(
        thinking_count(&agent.msg.messages),
        0,
        "the pair at index 2 was hidden, so every thinking block from there on is stale"
    );
}

#[tokio::test]
async fn mid_iteration_deferred_apply_ignores_count_pressure() {
    use zeph_agent_context::DeferredApplyTrigger;

    let mut agent = agent_with(
        MockProvider::with_responses(vec![]),
        MockToolExecutor::no_tools(),
    );
    let mut history = history_with_thinking();
    history[3].metadata.deferred_summary = Some("did read".into());
    agent.msg.messages = history;
    agent.services.memory.persistence.tool_call_cutoff = 1;
    let before = agent.msg.messages.len();

    agent.maybe_apply_deferred_summaries(DeferredApplyTrigger::TokenPressure);

    assert_eq!(
        agent.msg.messages.len(),
        before,
        "no token pressure, no edit"
    );
    assert_eq!(thinking_count(&agent.msg.messages), 1);
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn server_compaction_mirror_strips_the_responses_stale_thinking() {
    let response = ChatResponse::ToolUse {
        content: AssistantContent::new(vec![
            AssistantBlock::Thinking(thinking_block("pre-compaction")),
            AssistantBlock::ToolUse(call("c1", "read")),
        ]),
        replay: ThinkingReplay::Intact,
        compaction: Some("the earlier conversation".into()),
    };
    let (mock, _) = MockProvider::with_responses(vec![])
        .with_tool_use(vec![response, ChatResponse::Text("done".into())]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("read", "ok")))])
        .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor);
    agent.msg.messages = history_with_thinking();

    agent.process_response().await.unwrap();

    assert_eq!(thinking_count(&agent.msg.messages), 0);
    assert!(
        agent
            .msg
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, MessagePart::Compaction { .. })),
        "the mirror must have run"
    );
}

// --- History rewrites inside a turn: the thinking seam -------------------------------------

fn bash_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        description: String::new(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }
}

/// Thinking texts a Claude request built from `messages` would carry, in order.
fn claude_thinking_texts(messages: &[Message]) -> Vec<String> {
    let provider =
        zeph_llm::claude::ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 1024);
    let body = zeph_llm::LlmProvider::debug_request_json(
        &provider,
        messages,
        &[bash_tool_definition()],
        false,
    );
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "thinking")
        .map(|b| b["thinking"].as_str().unwrap().to_owned())
        .collect()
}

fn tool_step(index: usize, with_output_part: bool) -> [Message; 2] {
    let id = format!("c{index}");
    let result = if with_output_part {
        MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: "x".repeat(400),
            compacted_at: None,
        }
    } else {
        MessagePart::ToolResult {
            tool_use_id: id.clone(),
            content: "r".into(),
            is_error: false,
        }
    };
    [
        Message::from_parts(
            Role::Assistant,
            vec![
                thinking_part(&format!("t{index}")),
                MessagePart::ToolUse {
                    id,
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
        ),
        Message::from_parts(Role::User, vec![result]),
    ]
}

/// `[system, turn input, step 0 (output pruneable), step 1]` -- indexes 0..=5.
fn seam_history() -> Vec<Message> {
    let mut history = vec![
        Message::from_legacy(Role::System, "system"),
        user_turn("go"),
    ];
    history.extend(tool_step(0, true));
    history.extend(tool_step(1, false));
    history
}

/// Append the next iteration's response, as the loop does after an edit.
fn append_next_iteration(history: &mut Vec<Message>) {
    history.extend(tool_step(2, false));
}

fn seam_agent(history: Vec<Message>) -> Agent<MockChannel> {
    let mut agent = agent_with(
        MockProvider::with_responses(vec!["summary".into()]),
        MockToolExecutor::no_tools(),
    );
    agent.msg.messages = history;
    agent
}

#[tokio::test]
async fn soft_compaction_prune_strips_thinking_after_the_pruned_message_only() {
    let mut agent = seam_agent(seam_history()).with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;
    agent.runtime.providers.cached_prompt_tokens = 75_000;

    agent.maybe_soft_compact_mid_iteration();

    assert!(
        matches!(
            &agent.msg.messages[3].parts[0],
            MessagePart::ToolOutput {
                compacted_at: Some(_),
                ..
            }
        ),
        "the soft pass must have pruned the output"
    );

    assert_eq!(claude_thinking_texts(&agent.msg.messages), ["t0"]);
    append_next_iteration(&mut agent.msg.messages);
    assert_eq!(
        claude_thinking_texts(&agent.msg.messages),
        ["t0", "t2"],
        "the response after the edit keeps its thinking, in order"
    );
}

#[tokio::test]
async fn deferred_summary_insertion_strips_from_the_hidden_pair() {
    use zeph_agent_context::DeferredApplyTrigger;

    let mut history = seam_history();
    history[3].metadata.deferred_summary = Some("did bash".into());
    let mut agent = seam_agent(history);
    agent.services.memory.persistence.tool_call_cutoff = 1;

    agent.maybe_apply_deferred_summaries(DeferredApplyTrigger::TokenOrCount);

    assert_eq!(
        claude_thinking_texts(&agent.msg.messages),
        Vec::<String>::new()
    );
    append_next_iteration(&mut agent.msg.messages);
    assert_eq!(claude_thinking_texts(&agent.msg.messages), ["t2"]);
    assert!(
        agent.msg.messages[1].metadata.origin == Some(MessageOrigin::TurnInput),
        "nothing before the edit point is removed"
    );
}

#[tokio::test]
async fn llm_compaction_strips_thinking_from_the_preserved_tail() {
    let mut history = vec![Message::from_legacy(Role::System, "system")];
    for i in 0..10 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        history.push(Message::from_legacy(role, format!("filler {i}")));
    }
    history.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("tail"),
            MessagePart::Text {
                text: "tail answer".into(),
            },
        ],
    ));
    history.push(Message::from_legacy(Role::User, "tail question"));
    let mut agent = seam_agent(history).with_context_budget(10_000, 0.20, 0.80, 2, 0);

    let _ = agent.compact_context().await.unwrap();

    assert!(
        agent
            .msg
            .messages
            .iter()
            .any(|m| m.content.contains("summary")),
        "compaction must have run"
    );
    assert_eq!(thinking_count(&agent.msg.messages), 0);
}

#[tokio::test]
async fn knowledge_rebuild_strips_thinking_and_noop_rebuild_keeps_it() {
    let mut agent = seam_agent(seam_history());
    agent.rebuild_knowledge_block();
    assert_eq!(thinking_count(&agent.msg.messages), 2, "no block, no edit");

    agent.services.focus.append_llm_knowledge("a fact".into());
    agent.rebuild_knowledge_block();

    assert_eq!(thinking_count(&agent.msg.messages), 0);
    append_next_iteration(&mut agent.msg.messages);
    assert_eq!(claude_thinking_texts(&agent.msg.messages), ["t2"]);
}

#[tokio::test]
async fn complete_focus_strips_thinking_including_the_repushed_assistant_message() {
    let mut agent = seam_agent(vec![
        Message::from_legacy(Role::System, "system"),
        user_turn("go"),
    ]);
    let (_, checkpoint) =
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "s"}));
    agent.msg.messages.push(checkpoint.expect("checkpoint"));
    agent.msg.messages.extend(tool_step(0, false));
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("current"),
            MessagePart::ToolUse {
                id: "cf".into(),
                name: "complete_focus".into(),
                input: serde_json::json!({}),
            },
        ],
    ));

    let (result, _) = agent.handle_focus_tool(
        "complete_focus",
        &serde_json::json!({"summary": "found the bug"}),
    );

    assert!(!result.starts_with("[error]"), "{result}");
    let last = agent.msg.messages.last().unwrap();
    assert!(
        last.parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "cf")),
        "the current turn's assistant message is re-pushed"
    );
    assert_eq!(thinking_count(&agent.msg.messages), 0);
}

#[tokio::test]
async fn complete_focus_rejects_an_empty_summary_without_touching_history() {
    let mut agent = seam_agent(vec![
        Message::from_legacy(Role::System, "system"),
        user_turn("go"),
    ]);
    let (_, checkpoint) =
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "s"}));
    agent.msg.messages.push(checkpoint.expect("checkpoint"));
    agent.msg.messages.extend(tool_step(0, false));
    let before = agent.msg.messages.len();

    for input in [serde_json::json!({"summary": "   "}), serde_json::json!({})] {
        let (result, _) = agent.handle_focus_tool("complete_focus", &input);
        assert!(result.starts_with("[error]"), "{result}");
    }

    assert_eq!(agent.msg.messages.len(), before);
    assert!(agent.services.focus.is_active(), "the session stays open");
}

#[tokio::test]
async fn compress_context_strips_thinking_through_the_knowledge_rebuild() {
    let mut history = vec![Message::from_legacy(Role::System, "system")];
    for i in 0..14 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        history.push(Message::from_legacy(role, format!("filler {i}")));
    }
    history.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("kept-until-rebuild"),
            MessagePart::Text { text: "x".into() },
        ],
    ));
    let mut agent = seam_agent(history);

    let result = agent.handle_compress_context().await;

    assert!(result.starts_with("Compressed"), "{result}");
    assert_eq!(thinking_count(&agent.msg.messages), 0);
}

#[tokio::test]
async fn tombstone_insertion_strips_thinking_from_the_insert_index() {
    let mut history = vec![
        Message::from_legacy(Role::System, "system"),
        user_turn("go"),
    ];
    history.extend(tool_step(0, false));
    history.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("t1"),
            MessagePart::ToolUse {
                id: "orphan".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ],
    ));
    history.push(Message::from_parts(
        Role::Assistant,
        vec![
            thinking_part("later"),
            MessagePart::Text { text: "x".into() },
        ],
    ));
    let mut agent = seam_agent(history);

    agent
        .persist_cancelled_tool_results(&[call("orphan", "bash")], Some(5))
        .await;

    assert_eq!(thinking_count(&agent.msg.messages), 2, "only `later` goes");
    assert!(
        !agent.msg.messages[6]
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ThinkingBlock { .. })),
        "the message behind the inserted tombstone lost its thinking"
    );
}

#[tokio::test]
async fn turn_boundary_prune_leaves_thinking_alone() {
    let mut agent = seam_agent(seam_history());

    agent.prune_stale_tool_outputs(1);

    assert_eq!(
        thinking_count(&agent.msg.messages),
        2,
        "the boundary prune runs before the new turn has any thinking, so it needs no seam"
    );
}

// --- Gate state, trust and dependency bookkeeping ------------------------------------------

#[tokio::test]
#[allow(clippy::large_futures)]
async fn nested_process_response_restores_the_outer_turn_set() {
    use crate::agent::turn_gate::TurnToolSet;

    let (mock, _) = MockProvider::with_responses(vec!["inner".into()]).with_tool_use(vec![]);
    let mut agent = agent_with(mock, MockToolExecutor::no_tools());
    let defs: Vec<ToolDefinition> = vec![ToolDefinition {
        name: "read".into(),
        description: String::new(),
        parameters: serde_json::json!({}),
        output_schema: None,
    }];
    let outer = TurnToolSet::resolve(&defs, None, &HashSet::new());
    agent.services.tool_state.turn_tool_set = Some(outer);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());

    agent.process_response().await.unwrap();

    let gate = agent.dispatch_gate();
    assert!(
        gate.check("grep").is_err(),
        "membership is still enforced after the nested call"
    );
    assert!(gate.check("read").is_ok());
    assert!(
        gate.check("write").is_err(),
        "tools outside the set stay rejected; requires state is intact"
    );
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn skipped_prerequisite_does_not_unlock_its_dependant() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "read")]),
        ChatResponse::tool_use(None, vec![call("c2", "write")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![
        Ok(Some(zeph_tools::ToolOutput {
            claim_source: Some(zeph_tools::ClaimSource::Synthetic),
            ..output("read", "[skipped] by the utility gate")
        })),
        Ok(Some(output("write", "write ok"))),
    ])
    .with_definitions(vec![tool_def("read"), tool_def("write")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());

    agent.process_response().await.unwrap();

    assert!(agent.services.tool_state.completed_tool_ids.is_empty());
    let text = tool_result_text(&agent, "c2");
    assert!(text.contains("[error] Tool call to"), "{text}");
    assert!(!text.contains("write ok"), "{text}");
}

// --- Server compaction through the real provider wrapper chain ------------------------------

#[tokio::test]
#[allow(clippy::large_futures)]
async fn server_compaction_mirror_fires_through_the_masked_provider() {
    let response = ChatResponse::ToolUse {
        content: AssistantContent::new(vec![
            AssistantBlock::Thinking(thinking_block("stale")),
            AssistantBlock::ToolUse(call("c1", "read")),
        ]),
        replay: ThinkingReplay::Intact,
        compaction: Some("the earlier conversation".into()),
    };
    let (mock, _) = MockProvider::with_responses(vec![])
        .with_tool_use(vec![response, ChatResponse::Text("done".into())]);
    let executor = MockToolExecutor::new(vec![Ok(Some(output("read", "ok")))])
        .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor).with_secret_registry(Arc::new(
        zeph_sanitizer::secret_mask::SecretMaskRegistry::new(),
    ));
    assert!(
        matches!(agent.provider, AnyProvider::Masked(_)),
        "the default production chain wraps the provider"
    );
    agent.msg.messages = history_with_thinking();

    agent.process_response().await.unwrap();

    assert!(
        agent
            .msg
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, MessagePart::Compaction { .. })),
        "the mirror must run behind MaskedProvider"
    );
    assert_eq!(thinking_count(&agent.msg.messages), 0);
}

// --- Append-only requests across a real loop ------------------------------------------------

#[tokio::test]
#[allow(clippy::large_futures)]
async fn serialized_request_only_appends_between_loop_iterations() {
    let thinking_call = |id: &str, think: &str| ChatResponse::ToolUse {
        content: AssistantContent::new(vec![
            AssistantBlock::Thinking(thinking_block(think)),
            AssistantBlock::ToolUse(call(id, "read")),
        ]),
        replay: ThinkingReplay::Intact,
        compaction: None,
    };
    let (mock, recorded) = MockProvider::with_responses(vec![]).with_recording();
    let (mock, _) = mock.with_tool_use(vec![
        thinking_call("c1", "a"),
        thinking_call("c2", "b"),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![
        Ok(Some(output("read", "one"))),
        Ok(Some(output("read", "two"))),
    ])
    .with_definitions(vec![tool_def("read")]);
    let mut agent = agent_with(mock, executor);

    agent.process_response().await.unwrap();

    let provider =
        zeph_llm::claude::ClaudeProvider::new("k".into(), "claude-sonnet-5-5".into(), 1024)
            .with_cache_user_messages(false);
    let bodies: Vec<serde_json::Value> = recorded
        .lock()
        .unwrap()
        .iter()
        .map(|messages| {
            zeph_llm::LlmProvider::debug_request_json(
                &provider,
                messages,
                &[bash_tool_definition()],
                false,
            )
        })
        .collect();
    assert_eq!(bodies.len(), 3);
    for pair in bodies.windows(2) {
        let (old, new) = (
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
        );
        assert!(new.len() > old.len());
        assert_eq!(
            &new[..old.len()],
            old.as_slice(),
            "earlier messages must not change"
        );
    }
    let last = bodies[2]["messages"].to_string();
    assert!(last.contains("\"thinking\":\"a\"") && last.contains("\"thinking\":\"b\""));
    assert!(last.find("\"thinking\":\"a\"") < last.find("\"thinking\":\"b\""));
}

#[tokio::test]
async fn paste_gate_fails_closed_on_unmet_requires_without_a_turn_set() {
    let mut agent = agent_with(
        MockProvider::with_responses(vec![]),
        MockToolExecutor::no_tools(),
    );
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());
    assert!(agent.services.tool_state.turn_tool_set.is_none());

    let gate = agent.paste_dispatch_gate();

    assert!(gate.check("write").is_err());
    assert!(gate.check("read").is_ok());
    agent
        .services
        .tool_state
        .completed_tool_ids
        .insert("read".to_owned());
    assert!(agent.paste_dispatch_gate().check("write").is_ok());
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn another_caller_compacting_on_the_same_provider_does_not_touch_main_history() {
    let compacted = ChatResponse::ToolUse {
        content: AssistantContent::new(vec![AssistantBlock::ToolUse(call("sub1", "read"))]),
        replay: ThinkingReplay::Intact,
        compaction: Some("a subagent's summary".into()),
    };
    let (mock, _) = MockProvider::with_responses(vec![])
        .with_tool_use(vec![compacted, ChatResponse::Text("main answer".into())]);
    let mut agent = agent_with(mock, MockToolExecutor::no_tools());
    let before: Vec<String> = agent
        .msg
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect();

    // A subagent holds a clone of the same provider and receives the compacted response.
    let subagent_provider = agent.provider.clone();
    let sub = zeph_llm::LlmProvider::chat_with_tools(&subagent_provider, &[], &[])
        .await
        .unwrap();
    assert!(matches!(
        sub,
        ChatResponse::ToolUse {
            compaction: Some(_),
            ..
        }
    ));

    agent.process_response().await.unwrap();

    assert!(
        !agent
            .msg
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, MessagePart::Compaction { .. })),
        "the main loop must not mirror a summary that belongs to another caller"
    );
    let kept: Vec<String> = agent
        .msg
        .messages
        .iter()
        .take(before.len())
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(kept, before);
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn real_output_that_starts_with_a_skip_marker_still_unlocks_its_dependant() {
    let (mock, _) = MockProvider::with_responses(vec![]).with_tool_use(vec![
        ChatResponse::tool_use(None, vec![call("c1", "read")]),
        ChatResponse::tool_use(None, vec![call("c2", "write")]),
        ChatResponse::Text("done".into()),
    ]);
    let executor = MockToolExecutor::new(vec![
        Ok(Some(output(
            "read",
            "[stopped] service is stopped (real status output)",
        ))),
        Ok(Some(output("write", "write ok"))),
    ])
    .with_definitions(vec![tool_def("read"), tool_def("write")]);
    let mut agent = agent_with(mock, executor);
    agent.services.tool_state.dependency_graph = Some(graph_write_requires_read());

    agent.process_response().await.unwrap();

    assert!(tool_result_text(&agent, "c2").contains("write ok"));
}
