// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "mock")]

//! Acceptance tests (tests-first) for #7144: `McpManager::list_servers` returns typed
//! `McpServerId`s (configured order first), not raw strings.

use std::collections::HashMap;
use std::time::Duration;

use zeph_common::types::McpServerId;
use zeph_mcp::policy::PolicyEnforcer;
use zeph_mcp::tool::ToolSecurityMeta;
use zeph_mcp::{McpManager, McpTool, McpTransport, McpTrustLevel, ServerEntry};

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

#[tokio::test]
async fn list_servers_returns_typed_ids_in_configured_order() {
    let mgr = manager(&["zeta", "alpha", "mid"]);
    for id in ["mid", "alpha", "zeta"] {
        mgr.register_connected_for_testing(id, vec![tool(id, "t")])
            .await;
    }
    let listed: Vec<McpServerId> = mgr.list_servers().await;
    let expected: Vec<McpServerId> = ["zeta", "alpha", "mid"]
        .into_iter()
        .map(McpServerId::from)
        .collect();
    assert_eq!(listed, expected);
}

#[tokio::test]
async fn list_servers_typed_ids_are_unique_when_unconfigured_ids_connect() {
    let mgr = manager(&["b"]);
    mgr.register_connected_for_testing("b", vec![]).await;
    mgr.register_connected_for_testing("a", vec![]).await;
    let listed: Vec<McpServerId> = mgr.list_servers().await;
    assert_eq!(listed, [McpServerId::from("b"), McpServerId::from("a")]);
}
