// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for `McpToolScope` membership (#7083): `/mcp add x` admits a server's tools,
//! `/mcp remove x` stops admitting them, and `AllServers` ignores both.
//!
//! Registered from `tests/mod.rs` with `mod mcp_tool_scope_membership_tests;`.

use std::collections::HashSet;

use crate::agent::McpToolScope;
use zeph_common::types::McpServerId;
use zeph_mcp::McpTool;

fn tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: String::new(),
        input_schema: serde_json::json!({}),
        output_schema: None,
        security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
    }
}

fn scope(ids: &[&str]) -> McpToolScope {
    McpToolScope::Servers(
        ids.iter()
            .map(|s| McpServerId::from(*s))
            .collect::<HashSet<_>>(),
    )
}

fn servers(tools: &[McpTool]) -> Vec<&str> {
    tools.iter().map(|t| t.server_id.as_str()).collect()
}

#[test]
fn filter_servers_scope_keeps_only_listed_servers_in_order() {
    let all = vec![
        tool("b", "1"),
        tool("a", "2"),
        tool("c", "3"),
        tool("a", "4"),
    ];
    let kept = scope(&["a", "b"]).filter(all);
    assert_eq!(servers(&kept), ["b", "a", "a"]);
}

#[test]
fn filter_all_servers_scope_keeps_everything() {
    let all = vec![tool("a", "1"), tool("b", "2")];
    assert_eq!(McpToolScope::AllServers.filter(all).len(), 2);
}

#[test]
fn admit_makes_a_dynamically_added_servers_tools_visible() {
    let mut s = scope(&["a"]);
    let update = || vec![tool("a", "1"), tool("x", "2")];
    assert_eq!(servers(&s.filter(update())), ["a"]);

    s.admit("x".into());

    assert_eq!(servers(&s.filter(update())), ["a", "x"]);
}

#[test]
fn release_hides_a_removed_servers_tools_again() {
    let mut s = scope(&["a", "x"]);
    s.release("x");
    let kept = s.filter(vec![tool("a", "1"), tool("x", "2")]);
    assert_eq!(servers(&kept), ["a"]);
}

#[test]
fn admit_and_release_are_noops_for_all_servers() {
    let mut s = McpToolScope::AllServers;
    s.admit("x".into());
    s.release("y");
    assert_eq!(s, McpToolScope::AllServers);
}
