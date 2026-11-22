// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "mock")]

//! Acceptance tests (tests-first) for #7135 (native tool ids are pre-claimed when the MCP tool
//! list is published), #7118 (`list_servers` follows configured order) and #7138
//! (`register_connected_for_testing`).
//!
//! Assumed API: `zeph_mcp::tool_list::NativeToolIds` implements `Default` and
//! `FromIterator<ToolName>`; `McpToolListWriter::publish_full(&self, &[McpTool], &NativeToolIds)`;
//! `McpManager::register_connected_for_testing(&self, &str, Vec<McpTool>)` (async, `mock`-gated).

use std::collections::HashMap;
use std::time::Duration;

use zeph_common::ToolName;
use zeph_mcp::policy::PolicyEnforcer;
use zeph_mcp::tool::ToolSecurityMeta;
use zeph_mcp::tool_list::NativeToolIds;
use zeph_mcp::{McpManager, McpTool, McpToolList, McpTransport, McpTrustLevel, ServerEntry};

fn tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: String::new(),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        security_meta: ToolSecurityMeta::default(),
    }
}

fn native(ids: &[&str]) -> NativeToolIds {
    ids.iter().map(|id| ToolName::new(*id)).collect()
}

fn entry(id: &str) -> ServerEntry {
    ServerEntry {
        id: id.into(),
        transport: McpTransport::Stdio {
            command: "nonexistent-mcp-binary".into(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        timeout: Duration::from_secs(5),
        trust_level: McpTrustLevel::Untrusted,
        tool_allowlist: None,
        allow_untrusted_without_allowlist: false,
        expected_tools: Vec::new(),
        roots: Vec::new(),
        tool_metadata: HashMap::new(),
        elicitation_enabled: false,
        elicitation_timeout_secs: 120,
        env_isolation: false,
        media_passthrough: false,
    }
}

fn manager(order: &[&str]) -> McpManager {
    McpManager::new(
        order.iter().map(|id| entry(id)).collect(),
        vec![],
        PolicyEnforcer::new(vec![]),
    )
}

#[test]
fn publish_full_native_collision_is_absent_from_exposed_tools_and_ids() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(
        &[tool("memory", "search"), tool("memory", "other")],
        &native(&["memory_search"]),
    );
    let snap = reader.snapshot();
    let names: Vec<_> = snap.tools.iter().map(McpTool::qualified_name).collect();
    assert_eq!(names, ["memory:other"]);
    let ids = writer.ids_handle();
    assert!(
        !ids.contains("memory_search"),
        "native id must not be an MCP id"
    );
    assert!(ids.contains("memory_other"));
}

#[test]
fn publish_full_without_native_ids_keeps_every_tool() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(
        &[tool("memory", "search"), tool("memory", "other")],
        &NativeToolIds::default(),
    );
    assert_eq!(reader.snapshot().tools.len(), 2);
    assert!(writer.ids_handle().contains("memory_search"));
}

#[test]
fn publish_full_native_claim_beats_mcp_first_registration() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(
        &[tool("a.b", "c"), tool("a", "b_c"), tool("z", "t")],
        &native(&["a_b_c"]),
    );
    let names: Vec<_> = reader
        .snapshot()
        .tools
        .iter()
        .map(McpTool::qualified_name)
        .collect();
    assert_eq!(
        names,
        ["z:t"],
        "both colliding MCP tools lose to the native id"
    );
}

#[test]
fn publish_pruned_with_native_shadowed_tool_exposes_nothing_for_it() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(
        &[tool("memory", "search"), tool("memory", "other")],
        &native(&["memory_search"]),
    );
    writer.publish_pruned([tool("memory", "search"), tool("memory", "other")]);
    let names: Vec<_> = reader
        .snapshot()
        .tools
        .iter()
        .map(McpTool::qualified_name)
        .collect();
    assert_eq!(names, ["memory:other"]);
}

#[tokio::test]
async fn list_servers_follows_configured_order_not_alphabetical() {
    let mgr = manager(&["zeta", "alpha", "mid"]);
    for id in ["mid", "alpha", "zeta"] {
        mgr.register_connected_for_testing(id, vec![tool(id, "t")])
            .await;
    }
    assert_eq!(mgr.list_servers().await, ["zeta", "alpha", "mid"]);
}

#[tokio::test]
async fn list_servers_after_register_for_testing_reports_connected_state() {
    let mgr = manager(&["srv"]);
    assert_eq!(
        mgr.list_servers().await,
        Vec::<zeph_common::types::McpServerId>::new()
    );
    mgr.register_connected_for_testing("srv", vec![tool("srv", "t")])
        .await;
    assert_eq!(mgr.list_servers().await, ["srv"]);
    assert!(mgr.is_server_connected("srv"));
}

#[tokio::test]
async fn list_servers_skips_unconfigured_ids_without_dropping_them() {
    let mgr = manager(&["b"]);
    mgr.register_connected_for_testing("b", vec![]).await;
    mgr.register_connected_for_testing("a", vec![]).await;
    let listed = mgr.list_servers().await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], "b", "configured server comes first");
}
