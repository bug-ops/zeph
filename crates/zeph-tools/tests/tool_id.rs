// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7154: one canonical tool identity (`ToolMatchKey`) shared by the
//! sub-agent filter, hook matchers and the router.

use proptest::prelude::*;
use zeph_tools::tool_id::{SHELL_TOOL_ALIASES, ToolMatchKey, argument_pattern};

#[test]
fn test_tool_match_key_shell_spellings_share_the_runtime_id() {
    let bash = ToolMatchKey::new("bash");
    for raw in [
        "shell",
        "sh",
        "Shell",
        "SH ",
        " Bash ",
        "Bash(cargo *)",
        "shell(rm -rf *)",
    ] {
        assert_eq!(ToolMatchKey::new(raw), bash, "{raw:?}");
    }
    assert_eq!(bash.as_str(), "bash");
    assert_eq!(bash.clone().into_string(), "bash");
}

#[test]
fn test_tool_match_key_non_alias_ids_only_change_case_and_arguments() {
    assert_eq!(ToolMatchKey::new("Read").as_str(), "read");
    assert_eq!(ToolMatchKey::new("write(*.rs)").as_str(), "write");
    assert_eq!(
        ToolMatchKey::new("mcp__Server__Tool").as_str(),
        "mcp__server__tool"
    );
    assert_ne!(ToolMatchKey::new("read"), ToolMatchKey::new("write"));
}

#[test]
fn test_tool_match_key_empty_input_stays_empty() {
    assert_eq!(ToolMatchKey::new("").as_str(), "");
    assert_eq!(ToolMatchKey::new("  ").as_str(), "");
}

#[test]
fn test_argument_pattern_detects_entries_with_arguments_only() {
    assert!(argument_pattern("Bash(cargo *)").is_some());
    assert!(argument_pattern("bash").is_none());
    assert!(argument_pattern("").is_none());
}

#[test]
fn test_shell_tool_aliases_cover_bash_sh_shell() {
    let mut aliases = SHELL_TOOL_ALIASES.to_vec();
    aliases.sort_unstable();
    assert_eq!(aliases, ["bash", "sh", "shell"]);
}

proptest! {
    #[test]
    fn test_tool_match_key_is_idempotent(raw in "[A-Za-z_ ]{0,12}(\\([a-z *]{0,6}\\))?") {
        let key = ToolMatchKey::new(&raw);
        prop_assert_eq!(ToolMatchKey::new(key.as_str()), key);
    }

    #[test]
    fn test_tool_match_key_matches_agrees_with_new(
        key_raw in "[A-Za-zé_ ]{0,8}(\\([a-z *]{0,6}\\))?",
        raw in "[A-Za-zé_ ]{0,8}(\\([a-z *]{0,6}\\))?",
    ) {
        let key = ToolMatchKey::new(&key_raw);
        prop_assert_eq!(key.matches(&raw), ToolMatchKey::new(&raw) == key);
        prop_assert!(key.matches(&key_raw));
    }

    #[test]
    fn test_tool_match_key_matches_shell_aliases(
        alias in prop::sample::select(vec!["bash", "Sh", "SHELL", " shell (x)"]),
    ) {
        prop_assert!(ToolMatchKey::new("bash").matches(alias));
        prop_assert!(!ToolMatchKey::new("read").matches(alias));
    }
}
