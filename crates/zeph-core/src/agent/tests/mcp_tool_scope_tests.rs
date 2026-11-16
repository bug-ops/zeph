// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for per-agent MCP tool scoping (#7083).
//!
//! Registered from `tests/mod.rs` with `mod mcp_tool_scope_tests;`. Assumes
//! `Agent::with_mcp_tool_rx(rx, McpToolScope)` and `crate::agent::McpToolScope`.

use std::collections::HashSet;

use tokio::sync::watch;
use zeph_common::types::McpServerId;
use zeph_mcp::McpTool;

use crate::agent::McpToolScope;
use crate::agent::agent_tests::*;

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

fn agent_with(
    scope: McpToolScope,
    initial: Vec<McpTool>,
) -> (
    crate::agent::Agent<MockChannel>,
    watch::Sender<Vec<McpTool>>,
    watch::Receiver<MetricsSnapshot>,
) {
    let (tx, rx) = watch::channel(Vec::<McpTool>::new());
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = crate::agent::Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_metrics(metrics_tx)
    .with_mcp_tool_rx(rx, scope);
    agent.services.mcp.set_tools(initial);
    (agent, tx, metrics_rx)
}

fn names(agent: &crate::agent::Agent<MockChannel>) -> Vec<String> {
    agent
        .services
        .mcp
        .tools()
        .iter()
        .map(|t| format!("{}:{}", t.server_id, t.name))
        .collect()
}

#[tokio::test]
async fn check_tool_refresh_scoped_agent_ignores_updates_that_only_change_foreign_servers() {
    let (mut agent, tx, metrics) = agent_with(scope(&["a"]), vec![tool("a", "x")]);

    tx.send(vec![tool("a", "x"), tool("b", "y")]).unwrap();
    agent.check_tool_refresh().await;

    assert_eq!(names(&agent), ["a:x"], "tools unchanged");
    assert_eq!(
        metrics.borrow().mcp_tool_count,
        0,
        "a no-op refresh must not rebuild the index or touch metrics"
    );
}

#[tokio::test]
async fn check_tool_refresh_scoped_agent_applies_a_new_tool_on_a_watched_server() {
    let (mut agent, tx, _metrics) = agent_with(scope(&["a"]), vec![]);

    tx.send(vec![tool("a", "x"), tool("b", "y")]).unwrap();
    agent.check_tool_refresh().await;

    assert_eq!(names(&agent), ["a:x"]);
}

#[tokio::test]
async fn check_tool_refresh_scoped_agent_applies_added_tool_on_previously_toolless_server() {
    let (mut agent, tx, _metrics) = agent_with(scope(&["a", "b"]), vec![tool("a", "x")]);

    tx.send(vec![tool("a", "x"), tool("b", "z")]).unwrap();
    agent.check_tool_refresh().await;

    assert_eq!(names(&agent), ["a:x", "b:z"]);
}

#[tokio::test]
async fn check_tool_refresh_all_servers_scope_applies_every_update_as_before() {
    let (mut agent, tx, metrics) = agent_with(McpToolScope::AllServers, vec![]);

    tx.send(vec![tool("a", "x"), tool("b", "y")]).unwrap();
    agent.check_tool_refresh().await;

    assert_eq!(names(&agent), ["a:x", "b:y"]);
    assert_eq!(metrics.borrow().mcp_tool_count, 2);
}
