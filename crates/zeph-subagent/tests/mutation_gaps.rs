// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests added after mutation spot checks of the #7154/#7156/#7159 sub-agent changes: each one
//! pins a branch the first acceptance suite left unchecked.

use zeph_llm::ThinkingBinding;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_subagent::filter::{implicit_load_skill_scope, unknown_tool_entries};
use zeph_subagent::testing::{HistoryPins, trim_message_history};
use zeph_subagent::{DefinitionWarning, SubAgentDef, ToolListKind, ToolPolicy};

fn text(role: Role, s: &str) -> Message {
    Message::from_legacy(role, s)
}

fn tool_use(id: &str) -> Message {
    Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: id.into(),
            name: "bash".into(),
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

fn has_orphans(m: &[Message]) -> bool {
    let mut open: Vec<String> = Vec::new();
    let mut orphan = false;
    for message in m {
        for part in &message.parts {
            match part {
                MessagePart::ToolUse { id, .. } => open.push(id.to_string()),
                MessagePart::ToolResult { tool_use_id, .. } => {
                    orphan |= !open.contains(&tool_use_id.to_string());
                }
                _ => {}
            }
        }
    }
    orphan
}

#[test]
fn test_trim_message_history_pins_follow_task_when_repair_drops_orphan_result_before_it() {
    // Cutting the lone ToolUse leaves a ToolResult-only User message first in the window; repair
    // removes it, so every later index shifts down by one.
    let mut m = vec![
        text(Role::System, "sys"),
        tool_use("t1"),
        tool_result("t1"),
        text(Role::Assistant, "a2"),
        text(Role::User, "u2"),
        text(Role::Assistant, "a3"),
        text(Role::User, "TASK"),
    ];
    let mut pins = HistoryPins {
        system: true,
        original_task: None,
        current_task: 6,
    };
    trim_message_history(&mut m, 6, &mut pins, ThinkingBinding::Unbound);
    assert_eq!(m[0].content, "sys");
    assert_eq!(
        m[pins.current_task].content, "TASK",
        "{:?}",
        pins.current_task
    );
    assert_eq!(m.iter().filter(|x| x.content == "TASK").count(), 1);
    assert!(!has_orphans(&m));
}

#[test]
fn test_trim_message_history_pins_follow_both_tasks_when_repair_drops_orphan_tool_use() {
    // The trailing ToolUse has no result before RESUME; repair removes it ahead of both pins' tail.
    let mut m = vec![
        text(Role::System, "sys"),
        text(Role::User, "ORIG"),
        text(Role::Assistant, "a0"),
        text(Role::User, "u0"),
        text(Role::Assistant, "a1"),
        text(Role::User, "u1"),
        tool_use("t9"),
        text(Role::User, "RESUME"),
    ];
    let mut pins = HistoryPins {
        system: true,
        original_task: Some(1),
        current_task: 7,
    };
    trim_message_history(&mut m, 6, &mut pins, ThinkingBinding::Unbound);
    assert_eq!(m[0].content, "sys");
    assert_eq!(m[pins.original_task.expect("original pin")].content, "ORIG");
    assert_eq!(m[pins.current_task].content, "RESUME");
    assert!(!has_orphans(&m));
    assert!(
        m.len() < 6,
        "repair is expected to drop the emptied ToolUse message"
    );
}

#[test]
fn test_unknown_tool_entries_deny_list_entries_are_reported_as_deny() {
    let policy = ToolPolicy::DenyList(vec!["frobnicate".into(), "Shell".into()]);
    assert_eq!(
        unknown_tool_entries(&policy, &[], &["bash"]),
        vec![(ToolListKind::Deny, "frobnicate".to_owned())]
    );
}

#[test]
fn test_warnings_deny_list_argument_pattern_is_reported_as_deny() {
    let mut def = SubAgentDef::for_test("w");
    def.tools = ToolPolicy::DenyList(vec!["Bash(rm *)".into()]);
    assert_eq!(
        def.warnings(),
        vec![DefinitionWarning::ArgumentPatternIgnored {
            list: ToolListKind::Deny,
            entry: "Bash(rm *)".into(),
        }]
    );
}

#[test]
fn test_warnings_history_budget_boundary_is_two_per_turn_plus_two() {
    let warns = |history: usize| {
        let mut def = SubAgentDef::for_test("w");
        def.permissions.max_turns = 20;
        def.permissions.max_history_messages = history;
        !def.warnings().is_empty()
    };
    assert!(warns(41), "41 < 2*20+2");
    assert!(!warns(42), "42 == 2*20+2 fits");
    assert!(!warns(43));
    assert!(!warns(0), "0 disables the limit");
}

#[test]
fn test_implicit_scope_requires_both_include_and_matched_skills() {
    let mut def = SubAgentDef::for_test("d");
    def.tools = ToolPolicy::AllowList(vec!["read".into()]);

    def.skills.include = vec!["github".to_owned()];
    assert!(
        implicit_load_skill_scope(&def, &[]).is_none(),
        "nothing matched"
    );

    def.skills.include.clear();
    assert!(
        implicit_load_skill_scope(&def, &["github".to_owned()]).is_none(),
        "no include list"
    );
}
