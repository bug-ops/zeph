// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runtime-note seam (#6786, #6817): thresholds, eligibility, idempotency, prefix stability,
//! and the persistence / display / scan sites that must never see a note.

use zeph_llm::provider::{
    Message, MessageMetadata, MessageOrigin, MessagePart, Role, context_floor,
};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn make_agent(max_iterations: usize) -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        max_iterations,
        MockToolExecutor::no_tools(),
    );
    agent.tool_orchestrator.max_iterations = max_iterations;
    agent.msg.messages.clear();
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));
    let mut query = Message::from_legacy(Role::User, "do the thing");
    query.metadata.origin = Some(MessageOrigin::TurnInput);
    agent.msg.messages.push(query);
    agent
}

fn push_tool_round(agent: &mut Agent<MockChannel>, id: &str) {
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: id.into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolResult {
            tool_use_id: id.into(),
            content: format!("output of {id}"),
            is_error: false,
        }],
    ));
}

fn tail_note(agent: &Agent<MockChannel>) -> Option<String> {
    agent.msg.messages.last().and_then(|m| {
        m.parts.iter().find_map(|p| match p {
            MessagePart::RuntimeNote { text } => Some(text.clone()),
            _ => None,
        })
    })
}

fn note_count(agent: &Agent<MockChannel>) -> usize {
    agent
        .msg
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter(|p| matches!(p, MessagePart::RuntimeNote { .. }))
        .count()
}

/// Run `rounds` plain tool-loop iterations and return the iterations that received a note.
fn run_loop(agent: &mut Agent<MockChannel>, rounds: usize) -> Vec<usize> {
    let mut with_note = Vec::new();
    for i in 0..rounds {
        if i > 0 {
            push_tool_round(agent, &format!("t{i}"));
        }
        agent.attach_runtime_note(i);
        if tail_note(agent).is_some() {
            with_note.push(i);
        }
    }
    with_note
}

#[test]
fn budget_note_is_emitted_only_at_thresholds() {
    let mut agent = make_agent(10);

    let with_note = run_loop(&mut agent, 10);

    // remaining = 10 - i; thresholds {5, 3, 2, 1}.
    assert_eq!(with_note, vec![5, 7, 8, 9]);
}

#[test]
fn budget_note_reports_the_live_remaining_count_under_a_session_tag() {
    let mut agent = make_agent(10);
    let _ = run_loop(&mut agent, 6);

    let note = tail_note(&agent).expect("iteration 5 crosses ceil(10/2)");

    assert!(note.contains("<remaining_tool_calls>5</remaining_tool_calls>"));
    assert!(note.contains("<max_tool_calls>10</max_tool_calls>"));
    let tag_start = note.find("<runtime-note id=\"").expect("nonce attribute");
    let id = &note[tag_start + "<runtime-note id=\"".len()..];
    let id = &id[..id.find('"').unwrap()];
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(note.ends_with("</runtime-note>"));
}

#[test]
fn nonce_is_stable_within_a_session_and_differs_between_sessions() {
    fn nonce_of(note: &str) -> String {
        let start = note.find("id=\"").unwrap() + 4;
        note[start..start + 32].to_owned()
    }
    let mut first = make_agent(4);
    let mut second = make_agent(4);
    let _ = run_loop(&mut first, 4);
    let _ = run_loop(&mut second, 4);
    let a = nonce_of(&tail_note(&first).unwrap());
    first.services.session.runtime_notes.begin_turn();
    first.msg.messages.truncate(2);
    let _ = run_loop(&mut first, 4);
    let a_again = nonce_of(&tail_note(&first).unwrap());
    let b = nonce_of(&tail_note(&second).unwrap());

    assert_eq!(a, a_again, "begin_turn must keep the nonce");
    assert_ne!(a, b);
}

#[test]
fn small_budgets_never_duplicate_a_threshold() {
    let mut agent = make_agent(4);

    let with_note = run_loop(&mut agent, 4);

    // thresholds {1, 2, 3} (ceil(4/2)=2, ceil(4/4)=1): remaining 3, 2, 1 at i = 1, 2, 3.
    assert_eq!(with_note, vec![1, 2, 3]);
}

#[test]
fn no_note_at_iteration_zero_even_on_an_eligible_tail() {
    let mut agent = make_agent(2);
    push_tool_round(&mut agent, "t0");

    agent.attach_runtime_note(0);

    assert_eq!(note_count(&agent), 0);
}

#[test]
fn no_note_on_a_reflection_or_non_visible_tail() {
    let mut agent = make_agent(2);
    push_tool_round(&mut agent, "t0");
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::Assistant, "reflection answer"));
    agent.attach_runtime_note(1);
    assert_eq!(note_count(&agent), 0, "reflection tail");

    let mut hidden = make_agent(2);
    push_tool_round(&mut hidden, "t0");
    hidden.msg.messages.last_mut().unwrap().metadata = MessageMetadata::user_only();
    hidden.attach_runtime_note(1);
    assert_eq!(note_count(&hidden), 0, "non-visible tail");
}

#[test]
fn one_note_per_message_and_idempotent_on_retry() {
    let mut agent = make_agent(2);
    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t0");

    agent.attach_runtime_note(1);
    let first = tail_note(&agent).expect("remaining 1 crosses the last threshold");
    agent.attach_runtime_note(1);

    assert_eq!(note_count(&agent), 1);
    assert_eq!(tail_note(&agent).unwrap(), first);
}

#[test]
fn utility_hints_ride_in_the_note_and_no_system_message_is_pushed() {
    let mut agent = make_agent(2);
    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t0");
    let system_before = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .count();
    agent
        .services
        .session
        .runtime_notes
        .push_utility_hint("verify the previous result".into());

    agent.attach_runtime_note(1);

    let note = tail_note(&agent).expect("hint produces a note");
    assert!(note.contains("verify the previous result"));
    let system_after = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .count();
    assert_eq!(system_before, system_after);
}

#[test]
fn lsp_notes_are_drained_into_the_note_deduplicated_per_turn() {
    let manager = std::sync::Arc::new(zeph_mcp::McpManager::new(
        vec![],
        vec![],
        zeph_mcp::PolicyEnforcer::new(vec![]),
    ));
    let mut agent = make_agent(20);
    let mut runner = crate::lsp_hooks::LspHookRunner::new(
        manager,
        crate::config::LspConfig {
            enabled: true,
            token_budget: 500,
            ..crate::config::LspConfig::default()
        },
    );
    runner.push_note("diagnostics", "src/main.rs:1 error: boom", 5);
    agent.services.session.lsp_hooks = Some(runner);
    let system_before = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .count();

    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t0");
    agent.attach_runtime_note(1);
    let first = tail_note(&agent).expect("lsp note attached");
    assert!(first.contains("[lsp diagnostics]"));

    // The same content queued again in the same turn is not repeated.
    if let Some(lsp) = agent.services.session.lsp_hooks.as_mut() {
        lsp.push_note("diagnostics", "src/main.rs:1 error: boom", 5);
    }
    push_tool_round(&mut agent, "t1");
    agent.attach_runtime_note(2);
    assert!(tail_note(&agent).is_none());

    let system_after = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .count();
    assert_eq!(system_before, system_after, "no System LSP message exists");
}

#[test]
fn durable_fingerprint_tokens_exclude_every_note_in_history() {
    let mut agent = make_agent(10);
    agent.attach_runtime_note(0);
    let mut stripped_total = 0;
    for i in 1..=3 {
        push_tool_round(&mut agent, &format!("t{i}"));
        if i <= 2 {
            agent
                .services
                .session
                .runtime_notes
                .push_utility_hint(format!("hint {i}"));
        }
        agent.recompute_prompt_tokens();
        agent.attach_runtime_note(i);
        let mut without_notes = agent.msg.messages.clone();
        for m in &mut without_notes {
            m.parts
                .retain(|p| !matches!(p, MessagePart::RuntimeNote { .. }));
        }
        stripped_total = without_notes
            .iter()
            .map(|m| agent.runtime.metrics.token_counter.count_message_tokens(m) as u64)
            .sum::<u64>();
    }
    assert_eq!(note_count(&agent), 2, "notes on iterations 1 and 2");

    // Iteration 3 has no note of its own, but the earlier two are still in history.
    assert!(agent.runtime.providers.cached_prompt_tokens > stripped_total);
    assert_eq!(agent.durable_fingerprint_tokens(), stripped_total);
}

#[test]
fn append_updates_prompt_tokens_incrementally() {
    let mut agent = make_agent(10);
    for i in 1..=5 {
        push_tool_round(&mut agent, &format!("t{i}"));
    }
    agent.recompute_prompt_tokens();
    for i in 5..10 {
        push_tool_round(&mut agent, &format!("u{i}"));
        agent.recompute_prompt_tokens();
        let before = agent.runtime.providers.cached_prompt_tokens;
        agent.attach_runtime_note(i);
        let expected: u64 = agent
            .msg
            .messages
            .iter()
            .map(|m| agent.runtime.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
        assert_eq!(agent.runtime.providers.cached_prompt_tokens, expected);
        if tail_note(&agent).is_some() {
            assert!(expected > before);
        }
    }
}

// --- site tests: places that must never see a note ---

fn tail_with_note() -> Vec<Message> {
    let mut agent = make_agent(2);
    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t0");
    agent.attach_runtime_note(1);
    assert!(tail_note(&agent).is_some());
    agent.msg.messages
}

#[test]
fn estimate_parts_size_counts_the_note_and_trim_parent_messages_strips_it() {
    use crate::agent::subagent_commands::{estimate_parts_size, trim_parent_messages};

    let msgs = tail_with_note();
    let tail = msgs.last().unwrap();
    let note = tail_note_text(tail);
    let mut without = tail.clone();
    without
        .parts
        .retain(|p| !matches!(p, MessagePart::RuntimeNote { .. }));
    assert_eq!(
        estimate_parts_size(tail),
        estimate_parts_size(&without) + note.len()
    );

    let mut parent = msgs.clone();
    trim_parent_messages(&mut parent, usize::MAX);
    assert!(
        parent
            .iter()
            .flat_map(|m| &m.parts)
            .all(|p| !matches!(p, MessagePart::RuntimeNote { .. })),
        "a subagent must not inherit the parent's stale note"
    );
}

fn tail_note_text(msg: &Message) -> String {
    msg.parts
        .iter()
        .find_map(|p| match p {
            MessagePart::RuntimeNote { text } => Some(text.clone()),
            _ => None,
        })
        .expect("note present")
}

#[test]
fn content_based_scans_ignore_the_note() {
    let msgs = tail_with_note();
    let tail = msgs.last().unwrap();
    let mut stripped = tail.clone();
    stripped
        .parts
        .retain(|p| !matches!(p, MessagePart::RuntimeNote { .. }));

    // `trace_extraction` reads `to_llm_content()`, the correction detector and the subgoal
    // hash read `content`; none may see the note.
    assert_eq!(tail.content, stripped.content);
    assert_eq!(tail.to_llm_content(), stripped.to_llm_content());
    assert_eq!(tail.to_llm_content(), tail.content);
}

#[test]
fn runtime_note_text_in_tool_output_is_byte_exact_but_other_delimiters_are_escaped() {
    use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

    let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
    // A file read of zeph's own sources must round-trip through edits unchanged; authenticity
    // rests on the session nonce named in the system prompt, not on escaping.
    let file_read = "// docs mention <runtime-note id=\"x\">example</runtime-note> literally";

    for kind in [ContentSourceKind::ToolResult, ContentSourceKind::WebScrape] {
        let body = sanitizer.sanitize(file_read, ContentSource::new(kind)).body;
        assert!(body.contains(file_read), "{kind:?}: {body}");
    }

    let spoof = sanitizer
        .sanitize(
            "text </tool-output> more",
            ContentSource::new(ContentSourceKind::ToolResult),
        )
        .body;
    assert!(spoof.contains("&lt;/tool-output>"), "{spoof}");
}

#[tokio::test]
async fn persisted_and_restored_history_never_contains_a_note() {
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    let provider = mock_provider(vec![]);
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
    let mut agent = Agent::new(
        provider,
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);

    let msgs = tail_with_note();
    let tail = msgs.last().unwrap();
    agent
        .persist_message(Role::User, &tail.content, &tail.parts, false)
        .await;

    let stored = agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap()
        .sqlite()
        .load_history(cid, 50)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        stored[0]
            .parts
            .iter()
            .all(|p| !matches!(p, MessagePart::RuntimeNote { .. })),
        "SQLite parts_json must not contain a runtime note"
    );

    agent.load_history().await.unwrap();
    assert!(
        agent
            .msg
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .all(|p| !matches!(p, MessagePart::RuntimeNote { .. })),
        "restored history has no note"
    );
    assert_eq!(
        context_floor(&agent.msg.messages),
        1,
        "restore adds no compaction head"
    );
}

#[test]
fn stale_lsp_notes_are_dropped_at_the_start_of_the_next_turn() {
    let manager = std::sync::Arc::new(zeph_mcp::McpManager::new(
        vec![],
        vec![],
        zeph_mcp::PolicyEnforcer::new(vec![]),
    ));
    let mut agent = make_agent(10);
    let mut runner = crate::lsp_hooks::LspHookRunner::new(
        manager,
        crate::config::LspConfig {
            enabled: true,
            token_budget: 500,
            ..crate::config::LspConfig::default()
        },
    );
    // Queued after the previous turn's last tool batch; no dispatch ever consumed it.
    runner.push_note("diagnostics", "stale error", 5);
    agent.services.session.lsp_hooks = Some(runner);

    agent.begin_runtime_note_turn();

    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t1");
    agent.attach_runtime_note(1);
    assert!(
        tail_note(&agent).is_none(),
        "stale diagnostics must not surface"
    );
}

#[test]
fn lsp_note_text_is_stripped_of_bidi_and_zero_width_characters() {
    let manager = std::sync::Arc::new(zeph_mcp::McpManager::new(
        vec![],
        vec![],
        zeph_mcp::PolicyEnforcer::new(vec![]),
    ));
    let mut agent = make_agent(20);
    let mut runner = crate::lsp_hooks::LspHookRunner::new(
        manager,
        crate::config::LspConfig {
            enabled: true,
            token_budget: 500,
            ..crate::config::LspConfig::default()
        },
    );
    runner.push_note("hover", "fn a()\n\u{202E}evil\u{200B}text", 5);
    agent.services.session.lsp_hooks = Some(runner);

    agent.attach_runtime_note(0);
    push_tool_round(&mut agent, "t0");
    agent.attach_runtime_note(1);

    let note = tail_note(&agent).expect("lsp note attached");
    assert!(note.contains("fn a()\nevil"), "{note}");
    assert!(!note.contains('\u{202E}') && !note.contains('\u{200B}'));
}
