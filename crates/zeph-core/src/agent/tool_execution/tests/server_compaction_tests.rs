// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{
    Message, MessageMetadata, MessageOrigin, MessagePart, Role, context_floor,
};

use crate::agent::Agent;
use crate::agent::tests::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn make_agent() -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
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
        .push(Message::from_legacy(Role::System, "system prompt"));
    agent
}

fn turn_input(text: &str) -> Message {
    Message {
        metadata: MessageMetadata {
            origin: Some(MessageOrigin::TurnInput),
            ..MessageMetadata::default()
        },
        ..Message::from_legacy(Role::User, text)
    }
}

fn tool_use(id: &str) -> Message {
    Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: id.into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
    )
}

fn tool_result(id: &str) -> Message {
    Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: id.into(),
            content: "out".into(),
            is_error: false,
        }],
    )
}

fn knowledge_block() -> Message {
    Message {
        metadata: MessageMetadata::focus_pinned(),
        ..Message::from_legacy(Role::System, "[knowledge]")
    }
}

fn is_compaction(m: &Message) -> bool {
    m.parts
        .iter()
        .any(|p| matches!(p, MessagePart::Compaction { .. }))
}

fn mid_loop_agent() -> Agent<MockChannel> {
    let mut agent = make_agent();
    let msgs = &mut agent.msg.messages;
    msgs.push(knowledge_block());
    msgs.push(Message::from_legacy(Role::System, "[recall]"));
    msgs.push(Message::from_legacy(Role::User, "old turn"));
    msgs.push(Message::from_legacy(Role::Assistant, "old answer"));
    msgs.push(turn_input("current request"));
    msgs.push(tool_use("t1"));
    msgs.push(tool_result("t1"));
    agent
}

#[test]
fn mirror_keeps_system_knowledge_and_single_turn_input() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("the summary".to_owned());

    let msgs = &agent.msg.messages;
    let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::System,
            Role::System,
            Role::System,
            Role::Assistant,
            Role::User
        ]
    );
    assert_eq!(msgs[0].content, "system prompt");
    assert!(msgs[1].metadata.focus_pinned);
    assert_eq!(msgs[2].content, "[recall]");
    assert!(is_compaction(&msgs[3]));
    assert_eq!(msgs[4].content, "current request");
    assert_eq!(msgs[4].metadata.origin, Some(MessageOrigin::TurnInput));
}

#[test]
fn mirror_drops_mid_loop_tool_pair() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("s".to_owned());

    assert!(
        !agent
            .msg
            .messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(
                p,
                MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
            )))
    );
}

#[test]
fn two_consecutive_compactions_retain_only_latest_summary_and_turn_input() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("first".to_owned());

    agent.msg.supersede_turn_input();
    agent.msg.messages.push(turn_input("second request"));
    agent.msg.messages.push(tool_use("t2"));
    agent.msg.messages.push(tool_result("t2"));
    agent.apply_server_compaction("second".to_owned());

    let compactions: Vec<&Message> = agent
        .msg
        .messages
        .iter()
        .filter(|m| is_compaction(m))
        .collect();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].content, "second");
    let inputs: Vec<&Message> = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.metadata.origin == Some(MessageOrigin::TurnInput))
        .collect();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].content, "second request");
}

#[test]
fn mirror_recounts_prompt_tokens_and_non_system_count() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("s".to_owned());

    let expected: u64 = agent
        .msg
        .messages
        .iter()
        .map(|m| agent.runtime.metrics.token_counter.count_message_tokens(m) as u64)
        .sum();
    assert_eq!(agent.runtime.providers.cached_prompt_tokens, expected);
    assert_eq!(agent.msg.non_system_len(), 2);
}

#[tokio::test]
async fn system_prompt_rebuild_after_mirror_keeps_system_role() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("s".to_owned());

    agent.rebuild_system_prompt("hello").await;

    let head = &agent.msg.messages[0];
    assert_eq!(head.role, Role::System);
    assert!(head.parts.is_empty());
    assert!(agent.msg.messages.iter().any(is_compaction));
    assert!(
        agent
            .msg
            .messages
            .iter()
            .filter(|m| is_compaction(m))
            .all(|m| m.role == Role::Assistant && m.content == "s")
    );
}

#[test]
fn set_system_prompt_inserts_when_head_is_not_system() {
    let mut agent = make_agent();
    agent.msg.messages.clear();
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::Compaction {
            summary: "c".into(),
        }],
    ));

    agent.msg.set_system_prompt("fresh".to_owned());

    assert_eq!(agent.msg.messages.len(), 2);
    assert_eq!(agent.msg.messages[0].role, Role::System);
    assert_eq!(agent.msg.messages[0].content, "fresh");
    assert!(is_compaction(&agent.msg.messages[1]));
}

#[test]
fn set_system_prompt_overwrites_system_head_and_clears_parts() {
    let mut agent = make_agent();
    agent.msg.messages[0].parts = vec![MessagePart::Text { text: "x".into() }];

    agent.msg.set_system_prompt("fresh".to_owned());

    assert_eq!(agent.msg.messages.len(), 1);
    assert_eq!(agent.msg.messages[0].content, "fresh");
    assert!(agent.msg.messages[0].parts.is_empty());
}

#[test]
fn supersede_turn_input_clears_older_markers() {
    let mut agent = make_agent();
    agent.msg.messages.push(turn_input("a"));
    agent.msg.supersede_turn_input();
    assert!(
        agent
            .msg
            .messages
            .iter()
            .all(|m| m.metadata.origin.is_none())
    );
}

#[test]
fn focus_knowledge_block_is_inserted_after_compaction_floor() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("s".to_owned());
    agent
        .services
        .focus
        .append_llm_knowledge("learned fact".to_owned());

    agent.rebuild_knowledge_block();

    let floor = context_floor(&agent.msg.messages);
    assert!(is_compaction(&agent.msg.messages[floor - 1]));
    assert!(agent.msg.messages[floor].metadata.focus_pinned);
    assert_eq!(
        agent
            .msg
            .messages
            .iter()
            .filter(|m| m.metadata.focus_pinned)
            .count(),
        1
    );
}

#[test]
fn compress_context_selection_never_touches_compaction_block_or_turn_input() {
    let mut agent = mid_loop_agent();
    agent.apply_server_compaction("s".to_owned());
    for i in 0..12 {
        let role = if i % 2 == 0 {
            Role::Assistant
        } else {
            Role::User
        };
        agent
            .msg
            .messages
            .push(Message::from_legacy(role, format!("m{i}")));
    }

    let (indices, to_compress) = agent
        .select_messages_for_compression(2)
        .expect("enough messages");

    let floor = context_floor(&agent.msg.messages);
    assert!(indices.iter().all(|&i| i >= floor));
    assert!(to_compress.iter().all(|m| !is_compaction(m)));
    assert!(
        to_compress
            .iter()
            .all(|m| m.metadata.origin != Some(MessageOrigin::TurnInput))
    );
}

#[test]
fn server_compaction_prunes_subgoals_whose_messages_were_dropped() {
    use zeph_agent_context::SubgoalRegistry;

    let mut agent = mid_loop_agent();
    let done = agent
        .services
        .compression
        .subgoal_registry
        .push_active("done".to_owned());
    SubgoalRegistry::tag_untagged(&mut agent.msg.messages, 0..5, done);
    let active = agent
        .services
        .compression
        .subgoal_registry
        .push_active("active".to_owned());

    agent.apply_server_compaction("s".to_owned());

    let ids: Vec<_> = agent
        .services
        .compression
        .subgoal_registry
        .subgoals
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![active]);
}

#[test]
fn server_compaction_keeps_the_untrusted_context_gate_closed() {
    use zeph_sanitizer::ContentTrustLevel;

    let mut agent = mid_loop_agent();
    // A tagged tool-result message that the mirror drops (#6558: its trust tag must not vanish).
    let mut tagged = tool_result("t9");
    tagged.metadata.trust_level = Some(ContentTrustLevel::ExternalUntrusted as u8);
    agent.msg.messages.push(tagged);
    assert_eq!(
        agent.context_max_trust_level(),
        ContentTrustLevel::ExternalUntrusted
    );

    agent.apply_raw_server_compaction("summary distilled from a fetched page");

    assert!(
        agent
            .msg
            .messages
            .iter()
            .all(|m| m.metadata.trust_level.is_none() || is_compaction(m)),
        "only the compaction message carries the tag after the mirror"
    );
    assert_eq!(
        agent.context_max_trust_level(),
        ContentTrustLevel::ExternalUntrusted,
        "memory_save must keep its confirmation gate after a server compaction"
    );
}

#[test]
fn mirrored_compaction_block_is_labelled_llm_summary() {
    let mut agent = mid_loop_agent();

    assert!(agent.apply_raw_server_compaction("summary distilled from a fetched page"));

    let block = agent
        .msg
        .messages
        .iter()
        .find(|m| is_compaction(m))
        .expect("compaction message");
    assert!(
        block.content.contains("source=\"llm_summary\""),
        "{}",
        block.content
    );
    assert!(!block.content.contains("mcp_response"));
}

#[test]
fn head_injected_user_messages_are_never_subgoal_tagged() {
    use zeph_agent_context::SubgoalRegistry;

    let mut agent = make_agent();
    agent.msg.messages.push(turn_input("question"));
    // Tiered recall and the session digest are User-role head injections at the floor.
    zeph_llm::provider::insert_at_context_floor(
        &mut agent.msg.messages,
        Message::from_legacy(Role::User, "[recall] something"),
    );
    let id = agent
        .services
        .compression
        .subgoal_registry
        .push_active("g".to_owned());

    let len = agent.msg.messages.len();
    SubgoalRegistry::tag_untagged(&mut agent.msg.messages, 0..len, id);

    assert!(
        agent
            .msg
            .messages
            .iter()
            .all(|m| m.metadata.subgoal_id.is_none()),
        "head injections and the turn input stay untagged"
    );
}

#[tokio::test]
async fn inject_summaries_shim_lands_after_the_compaction_block() {
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
        "test-model",
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .sqlite()
        .save_summary(cid, "earlier work", None, None, 5)
        .await
        .unwrap();
    let mut agent = mid_loop_agent().with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
    agent.apply_server_compaction("s".to_owned());

    agent.inject_summaries(1000).await.unwrap();

    let floor = context_floor(&agent.msg.messages);
    assert!(is_compaction(&agent.msg.messages[floor - 1]));
    assert!(agent.msg.messages[floor].content.contains("earlier work"));
}
