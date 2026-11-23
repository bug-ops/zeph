// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7172: `/mcp list` lists the loaded tools from `metrics.mcp_tools`.
//!
//! Needs `#[cfg(test)] mod mcp_list_tests;` in `app/mod.rs`.

use tokio::sync::mpsc;

use super::App;

fn app() -> App {
    let (user_tx, _user_rx) = mpsc::channel(4);
    let (_agent_tx, agent_rx) = mpsc::channel(4);
    App::new(user_tx, agent_rx)
}

#[test]
fn format_mcp_list_lists_exactly_the_loaded_tools() {
    let mut app = app();
    app.metrics.mcp_server_count = 1;
    app.metrics.mcp_tools = vec!["gh_issues".to_owned(), "gh_prs".to_owned()];
    let out = app.format_mcp_list();
    assert!(out.contains("gh_issues") && out.contains("gh_prs"), "{out}");
    assert!(out.contains("Tools (2)"), "{out}");
    assert!(!out.contains("No MCP tools available."), "{out}");
}

#[test]
fn format_mcp_list_reports_none_only_when_no_tools_are_loaded() {
    let app = app();
    assert_eq!(app.format_mcp_list(), "No MCP tools available.");
}
