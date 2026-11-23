// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7154: sub-agent tool restrictions, hook matchers and definition
//! warnings must treat `shell`/`sh`/`bash` as one tool and report ignored entries.

mod common;

use common::{call, stub_box};
use proptest::prelude::*;
use zeph_subagent::filter::{policy_allows, unknown_tool_entries};
use zeph_subagent::{
    DefinitionWarning, FilteredToolExecutor, HookAction, HookDef, HookEvent, HookMatcher,
    SubAgentDef, ToolListKind, ToolPolicy, hook_if_matches, matching_hooks, normalize_tool_id,
};
use zeph_tools::executor::{ErasedToolExecutor, ToolError};

fn hook() -> HookDef {
    HookDef {
        action: HookAction::Command {
            command: "true".to_owned(),
        },
        timeout_secs: 30,
        fail_closed: false,
        r#if: None,
    }
}

fn matcher(m: &str) -> Vec<HookMatcher> {
    vec![HookMatcher {
        matcher: m.to_owned(),
        hooks: vec![hook()],
    }]
}

#[test]
fn test_normalize_tool_id_resolves_shell_aliases_to_runtime_id() {
    for alias in ["shell", "sh", "Shell", " SH ", "Bash(cargo *)", "bash"] {
        assert_eq!(normalize_tool_id(alias), "bash", "alias {alias:?}");
    }
    assert_eq!(normalize_tool_id("Read"), "read");
}

#[tokio::test]
async fn test_filtered_executor_deny_list_shell_blocks_bash() {
    let exec = FilteredToolExecutor::new(
        stub_box(&["bash", "read"]),
        ToolPolicy::DenyList(vec!["shell".into()]),
    );
    let err = exec.execute_tool_call_erased(&call("bash")).await;
    assert!(matches!(err, Err(ToolError::Blocked { .. })));
    assert!(exec.execute_tool_call_erased(&call("read")).await.is_ok());
}

#[tokio::test]
async fn test_filtered_executor_allow_list_shell_keeps_bash() {
    let exec = FilteredToolExecutor::new(
        stub_box(&["bash", "read"]),
        ToolPolicy::AllowList(vec!["shell".into()]),
    );
    assert!(exec.execute_tool_call_erased(&call("bash")).await.is_ok());
    let ids: Vec<_> = exec
        .tool_definitions_erased()
        .into_iter()
        .map(|d| d.id.to_string())
        .collect();
    assert_eq!(ids, ["bash"]);
}

#[tokio::test]
async fn test_filtered_executor_except_shell_blocks_bash_over_allow() {
    let exec = FilteredToolExecutor::with_disallowed(
        stub_box(&["bash", "read"]),
        ToolPolicy::InheritAll,
        vec!["sh".into()],
    );
    assert!(matches!(
        exec.execute_tool_call_erased(&call("bash")).await,
        Err(ToolError::Blocked { .. })
    ));
}

#[test]
fn test_policy_allows_deny_entries_beat_allow_and_use_aliases() {
    let allow = ToolPolicy::AllowList(vec!["shell".into(), "read".into()]);
    assert!(policy_allows(&allow, &[], "bash"));
    assert!(!policy_allows(&allow, &["Shell".to_owned()], "bash"));
    assert!(!policy_allows(&allow, &[], "write"));
    assert!(policy_allows(&ToolPolicy::InheritAll, &[], "anything"));
    assert!(!policy_allows(
        &ToolPolicy::DenyList(vec!["sh".into()]),
        &[],
        "bash"
    ));
}

#[test]
fn test_matching_hooks_documented_names_fire_for_runtime_tools() {
    assert_eq!(matching_hooks(&matcher("Bash"), "bash").len(), 1);
    assert_eq!(matching_hooks(&matcher("Shell"), "bash").len(), 1);
    assert_eq!(matching_hooks(&matcher("Edit|Write"), "edit").len(), 1);
    assert_eq!(matching_hooks(&matcher("Edit|Write"), "write").len(), 1);
    assert_eq!(matching_hooks(&matcher("Bash(cargo *)"), "bash").len(), 1);
}

#[test]
fn test_matching_hooks_unrelated_tool_does_not_fire() {
    assert!(matching_hooks(&matcher("Bash"), "read").is_empty());
    assert!(matching_hooks(&matcher("Edit|Write"), "bash").is_empty());
}

#[test]
fn test_hook_if_matches_tool_key_is_alias_aware() {
    assert!(hook_if_matches("tool:shell", Some("bash")));
    assert!(hook_if_matches("tool:Bash", Some("bash")));
    assert!(!hook_if_matches("tool:shell", Some("read")));
    assert!(!hook_if_matches("tool:shell", None));
}

proptest! {
    #[test]
    fn test_matching_hooks_is_superset_of_legacy_substring_rule(
        matcher_str in "[A-Za-z_|]{0,12}",
        tool in "[a-z_]{1,10}",
    ) {
        let legacy = matcher_str
            .split('|')
            .filter(|t| !t.is_empty())
            .any(|t| tool.contains(t));
        let now = !matching_hooks(&matcher(&matcher_str), &tool).is_empty();
        prop_assert!(!legacy || now);
    }
}

#[test]
fn test_warnings_argument_pattern_in_tool_lists() {
    let mut def = SubAgentDef::for_test("w");
    def.tools = ToolPolicy::AllowList(vec!["Bash(cargo *)".into(), "read".into()]);
    def.disallowed_tools = vec!["write(*.rs)".into()];
    let warnings = def.warnings();
    assert!(
        warnings.contains(&DefinitionWarning::ArgumentPatternIgnored {
            list: ToolListKind::Allow,
            entry: "Bash(cargo *)".into(),
        })
    );
    assert!(
        warnings.contains(&DefinitionWarning::ArgumentPatternIgnored {
            list: ToolListKind::Except,
            entry: "write(*.rs)".into(),
        })
    );
    assert_eq!(warnings.len(), 2);
}

#[test]
fn test_warnings_hook_matcher_argument_ignored() {
    let mut def = SubAgentDef::for_test("w");
    def.hooks.pre_tool_use = matcher("Bash(cargo *)|Edit");
    assert_eq!(
        def.warnings(),
        vec![DefinitionWarning::HookMatcherArgumentIgnored {
            event: HookEvent::PreToolUse,
            token: "Bash(cargo *)".into(),
        }]
    );
}

#[test]
fn test_warnings_history_budget_too_small_for_max_turns() {
    let mut def = SubAgentDef::for_test("w");
    def.permissions.max_turns = 20;
    def.permissions.max_history_messages = 20;
    assert_eq!(
        def.warnings(),
        vec![DefinitionWarning::HistoryBudgetTooSmall {
            max_turns: 20,
            max_history_messages: 20,
        }]
    );
}

#[test]
fn test_warnings_empty_for_clean_definition_and_unlimited_history() {
    let mut def = SubAgentDef::for_test("w");
    def.tools = ToolPolicy::AllowList(vec!["read".into(), "bash".into()]);
    assert_eq!(def.warnings(), Vec::new());
    def.permissions.max_turns = 1000;
    def.permissions.max_history_messages = 0;
    assert_eq!(def.warnings(), Vec::new());
    def.permissions.max_history_messages = 2 * 1000 + 2;
    assert_eq!(def.warnings(), Vec::new());
}

#[test]
fn test_unknown_tool_entries_reports_entries_matching_no_registered_tool() {
    let policy = ToolPolicy::AllowList(vec!["shell".into(), "frobnicate".into()]);
    let unknown = unknown_tool_entries(&policy, &["nope".to_owned()], &["bash", "read"]);
    assert_eq!(
        unknown,
        vec![
            (ToolListKind::Allow, "frobnicate".to_owned()),
            (ToolListKind::GlobalDisallowed, "nope".to_owned()),
        ]
    );
}

#[test]
fn test_unknown_tool_entries_alias_and_case_match_registered_tool() {
    let policy = ToolPolicy::DenyList(vec!["Shell".into(), "READ".into()]);
    assert_eq!(
        unknown_tool_entries(&policy, &[], &["bash", "read"]),
        Vec::new()
    );
}
