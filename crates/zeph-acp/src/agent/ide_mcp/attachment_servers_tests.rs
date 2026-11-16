// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for `IdeMcpAttachment.servers` (#7083): the session's tool scope must admit
//! every attached server, including those that expose no tools yet.
//!
//! Register in `ide_mcp.rs` with `#[cfg(test)] mod attachment_servers_tests;`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use parking_lot::Mutex as SyncMutex;

use super::*;

#[derive(Default)]
struct Host {
    connected: SyncMutex<HashSet<String>>,
    failing: SyncMutex<HashSet<String>>,
}

impl IdeMcpHost for Host {
    fn add_server(
        &self,
        entry: &ServerEntry,
    ) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
        let id = entry.id.to_string();
        let result = if self.failing.lock().contains(&id) {
            Err(McpError::ServerNotFound { server_id: id })
        } else {
            self.connected.lock().insert(id);
            // A server that connected but exposes no tools (no operator grant).
            Ok(Vec::new())
        };
        std::future::ready(result)
    }

    fn remove_server(&self, server_id: &str) -> impl Future<Output = Result<(), McpError>> + Send {
        self.connected.lock().remove(server_id);
        std::future::ready(Ok(()))
    }
}

fn entry(id: &str) -> ServerEntry {
    ServerEntry {
        id: id.into(),
        transport: zeph_mcp::McpTransport::Stdio {
            command: "unused".to_owned(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        timeout: Duration::from_secs(1),
        trust_level: zeph_mcp::McpTrustLevel::Untrusted,
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

#[tokio::test]
async fn attach_reports_toolless_servers_in_servers() {
    let registry = IdeMcpRegistry::new();
    let host = Host::default();
    let attachment = registry
        .attach(&host, vec![entry("a-1"), entry("b-1")])
        .await;
    assert!(attachment.tools.is_empty());
    let ids: Vec<&str> = attachment.servers.iter().map(McpServerId::as_str).collect();
    assert_eq!(ids, ["a-1", "b-1"]);
}

#[tokio::test]
async fn attach_does_not_report_failed_servers_in_servers() {
    let registry = IdeMcpRegistry::new();
    let host = Host::default();
    host.failing.lock().insert("bad-1".to_owned());
    let attachment = registry
        .attach(&host, vec![entry("bad-1"), entry("ok-1")])
        .await;
    let ids: Vec<&str> = attachment.servers.iter().map(McpServerId::as_str).collect();
    assert_eq!(ids, ["ok-1"]);
}
