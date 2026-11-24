// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7178: the tools view lists the loaded tools from `metrics.mcp_tools`,
//! not the per-turn `active_mcp_tools`.
//!
//! Needs `#[cfg(test)] mod tools_view_tests;` in `app/mod.rs`.

use tokio::sync::mpsc;

use super::App;

fn app() -> App {
    let (user_tx, _user_rx) = mpsc::channel(4);
    let (_agent_tx, agent_rx) = mpsc::channel(4);
    App::new(user_tx, agent_rx)
}

#[test]
fn format_tool_list_lists_loaded_tools_before_any_turn() {
    let mut app = app();
    app.metrics.mcp_tools = vec!["gh_issues".to_owned(), "gh_prs".to_owned()].into();
    assert_eq!(app.metrics.active_mcp_tools, [] as [String; 0]);
    let out = app.format_tool_list();
    assert!(out.contains("gh_issues") && out.contains("gh_prs"), "{out}");
    assert!(out.contains("(2)"), "{out}");
}

#[test]
fn format_tool_list_reports_none_only_when_nothing_is_loaded() {
    assert_eq!(app().format_tool_list(), "No tools available.");
}
