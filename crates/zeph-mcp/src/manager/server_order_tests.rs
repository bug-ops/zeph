// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for deterministic MCP tool order (#7087).
//!
//! Registered from `manager/mod.rs` with `#[cfg(test)] mod server_order_tests;`. Requires
//! `tools_in_server_order` to be reachable as `super::tools_in_server_order`.

use super::*;
use crate::tool::{McpTool, ToolSecurityMeta};
use crate::tool_list::McpToolList;
use zeph_common::types::McpServerId;

fn tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: String::new(),
        input_schema: serde_json::json!({}),
        output_schema: None,
        security_meta: ToolSecurityMeta::default(),
    }
}

fn sid(s: &str) -> McpServerId {
    McpServerId::from(s)
}

fn map_in_order(order: &[&str]) -> HashMap<McpServerId, Vec<McpTool>> {
    let mut map = HashMap::new();
    for server in order {
        // "a.b"/"c" and "a"/"b_c" both sanitize to "a_b_c".
        let tools = match *server {
            "a.b" => vec![tool("a.b", "c")],
            "a" => vec![tool("a", "b_c")],
            other => vec![tool(other, "t")],
        };
        map.insert(sid(server), tools);
    }
    map
}

fn qualified(tools: &[McpTool]) -> Vec<String> {
    tools.iter().map(McpTool::qualified_name).collect()
}

#[test]
fn tools_in_server_order_is_independent_of_map_insertion_order() {
    let configured = [sid("a.b"), sid("a")];
    let forward = tools_in_server_order(&map_in_order(&["a.b", "a"]), &configured);
    let backward = tools_in_server_order(&map_in_order(&["a", "a.b"]), &configured);
    assert_eq!(qualified(&forward), qualified(&backward));
    assert_eq!(qualified(&forward), ["a.b:c", "a:b_c"]);
}

#[test]
fn tools_in_server_order_colliding_id_resolves_to_configured_first_in_both_orders() {
    for configured in [[sid("a.b"), sid("a")], [sid("a"), sid("a.b")]] {
        for insertion in [["a.b", "a"], ["a", "a.b"]] {
            let ordered = tools_in_server_order(&map_in_order(&insertion), &configured);
            let (writer, _reader) = McpToolList::new(ordered.clone());
            let ids = writer.ids_handle().snapshot();
            let winner = ids.server_id("a_b_c").map(|s| s.as_str().to_owned());
            assert_eq!(winner.as_deref(), Some(configured[0].as_str()));
            assert_eq!(ordered[0].server_id, configured[0]);
        }
    }
}

#[test]
fn tools_in_server_order_puts_configured_first_then_dynamic_ids_ascending() {
    let configured = [sid("zeta"), sid("alpha")];
    let map = map_in_order(&["dyn-b", "alpha", "dyn-a", "zeta", "dyn-c"]);
    let ordered = tools_in_server_order(&map, &configured);
    let servers: Vec<&str> = ordered.iter().map(|t| t.server_id.as_str()).collect();
    assert_eq!(servers, ["zeta", "alpha", "dyn-a", "dyn-b", "dyn-c"]);
}

#[test]
fn tools_in_server_order_ignores_configured_ids_without_tools() {
    let configured = [sid("missing"), sid("a")];
    let ordered = tools_in_server_order(&map_in_order(&["a"]), &configured);
    assert_eq!(qualified(&ordered), ["a:b_c"]);
}

fn failing_entry(id: &str) -> ServerEntry {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_all_outcomes_follow_configured_order() {
    // Many servers in a non-alphabetical order, so completion order and map order both differ
    // from the configured order unless the outcomes are sorted by it.
    let ids: Vec<String> = (0..48)
        .map(|i| format!("srv-{:02}", (i * 17) % 48))
        .collect();
    let mgr = McpManager::new(
        ids.iter().map(|id| failing_entry(id)).collect(),
        vec![],
        PolicyEnforcer::new(vec![]),
    );
    let (_tools, outcomes) = mgr.connect_all().await;
    let got: Vec<String> = outcomes.iter().map(|o| o.id.to_string()).collect();
    assert_eq!(got, ids);
}
