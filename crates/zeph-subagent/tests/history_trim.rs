// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7156: sub-agent history trimming never evicts the system message or
//! the task prompts.
//!
//! Needs the `#[doc(hidden)]` re-export `zeph_subagent::testing::{trim_message_history,
//! HistoryPins}` (see handoff stub list).

use zeph_llm::ThinkingBinding;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_subagent::testing::{HistoryPins, trim_message_history};

fn msg(role: Role, text: &str) -> Message {
    Message::from_legacy(role, text)
}

/// Alternating history: odd indexes User, even Assistant, texts `h0..`.
fn alternating(n: usize, first: Role) -> Vec<Message> {
    (0..n)
        .map(|i| {
            let role = if (i % 2 == 0) == (first == Role::User) {
                Role::User
            } else {
                Role::Assistant
            };
            msg(role, &format!("h{i}"))
        })
        .collect()
}

fn texts(m: &[Message]) -> Vec<&str> {
    m.iter().map(|m| m.content.as_str()).collect()
}

fn count(m: &[Message], text: &str) -> usize {
    m.iter().filter(|m| m.content == text).count()
}

fn no_adjacent_users(m: &[Message]) -> bool {
    m.windows(2)
        .all(|w| !(w[0].role == Role::User && w[1].role == Role::User))
}

#[test]
fn test_trim_message_history_noop_when_limit_zero_or_not_exceeded() {
    let mut m = vec![msg(Role::System, "sys"), msg(Role::User, "task")];
    let mut pins = HistoryPins {
        system: true,
        original_task: None,
        current_task: 1,
    };
    trim_message_history(&mut m, 0, &mut pins, ThinkingBinding::Unbound);
    trim_message_history(&mut m, 2, &mut pins, ThinkingBinding::Unbound);
    assert_eq!(texts(&m), ["sys", "task"]);
}

#[test]
fn test_trim_message_history_keeps_task_with_limit_20_over_15_turns() {
    let mut m = vec![msg(Role::System, "sys"), msg(Role::User, "TASK")];
    let mut pins = HistoryPins {
        system: true,
        original_task: None,
        current_task: 1,
    };
    for turn in 0..15 {
        m.push(msg(Role::Assistant, &format!("a{turn}")));
        m.push(msg(Role::User, &format!("u{turn}")));
        trim_message_history(&mut m, 20, &mut pins, ThinkingBinding::Unbound);
    }
    assert_eq!(m[0].content, "sys");
    assert_eq!(count(&m, "TASK"), 1, "task must survive: {:?}", texts(&m));
    assert_eq!(m[pins.current_task].content, "TASK");
    assert!(m.len() <= 20);
    assert!(m.last().is_some_and(|l| l.content == "u14"));
}

#[test]
fn test_trim_message_history_fresh_one_over_drops_only_oldest_parent_message() {
    // [sys, p0..p5 (User first), TASK]
    let mut m = vec![msg(Role::System, "sys")];
    m.extend(alternating(6, Role::User));
    m.push(msg(Role::User, "TASK"));
    let mut pins = HistoryPins {
        system: true,
        original_task: None,
        current_task: 7,
    };
    trim_message_history(&mut m, 7, &mut pins, ThinkingBinding::Unbound);
    assert_eq!(m[0].content, "sys");
    assert_eq!(count(&m, "h0"), 0);
    assert_eq!(count(&m, "TASK"), 1);
    assert!(m.len() <= 7);
    assert_eq!(m[pins.current_task].content, "TASK");
    assert!(
        count(&m, "h5") == 1,
        "newest parent message must be kept: {:?}",
        texts(&m)
    );
}

#[test]
fn test_trim_message_history_resume_keeps_original_task_and_resume_prompt() {
    // [sys, ORIG, transcript..., RESUME]
    let mut m = vec![msg(Role::System, "sys"), msg(Role::User, "ORIG")];
    m.extend(alternating(29, Role::Assistant));
    m.push(msg(Role::User, "RESUME"));
    let last = m.len() - 1;
    let mut pins = HistoryPins {
        system: true,
        original_task: Some(1),
        current_task: last,
    };
    trim_message_history(&mut m, 20, &mut pins, ThinkingBinding::Unbound);
    assert_eq!(m[0].content, "sys");
    assert_eq!(m[1].content, "ORIG");
    assert_eq!(count(&m, "RESUME"), 1);
    assert_eq!(m[pins.current_task].content, "RESUME");
    assert!(m.len() <= 20);
    assert_eq!(count(&m, "h0"), 0, "oldest prior turns go first");
    assert!(no_adjacent_users(&m));
}

#[test]
fn test_trim_message_history_resume_tiny_limit_never_adjoins_pinned_users() {
    let mut m = vec![msg(Role::System, "sys"), msg(Role::User, "ORIG")];
    m.extend(alternating(10, Role::Assistant));
    m.push(msg(Role::User, "RESUME"));
    let last = m.len() - 1;
    let mut pins = HistoryPins {
        system: true,
        original_task: Some(1),
        current_task: last,
    };
    trim_message_history(&mut m, 3, &mut pins, ThinkingBinding::Unbound);
    assert!(m.len() <= 6, "floor is pins + 3: {:?}", texts(&m));
    assert_eq!(count(&m, "ORIG"), 1);
    assert_eq!(count(&m, "RESUME"), 1);
    assert!(no_adjacent_users(&m), "{:?}", texts(&m));
}

#[test]
fn test_trim_message_history_leaves_no_orphaned_tool_results() {
    let mut m = vec![msg(Role::System, "sys"), msg(Role::User, "TASK")];
    for i in 0..6 {
        let id = format!("t{i}");
        m.push(Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: id.as_str().into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
        ));
        m.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: id.as_str().into(),
                content: "out".into(),
                is_error: false,
            }],
        ));
    }
    let mut pins = HistoryPins {
        system: true,
        original_task: None,
        current_task: 1,
    };
    trim_message_history(&mut m, 6, &mut pins, ThinkingBinding::Unbound);
    let mut open: Vec<String> = Vec::new();
    for message in &m {
        for part in &message.parts {
            match part {
                MessagePart::ToolUse { id, .. } => open.push(id.to_string()),
                MessagePart::ToolResult { tool_use_id, .. } => assert!(
                    open.contains(&tool_use_id.to_string()),
                    "orphaned ToolResult {tool_use_id}"
                ),
                _ => {}
            }
        }
    }
    assert_eq!(count(&m, "TASK"), 1);
}
