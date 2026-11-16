// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance test for the idempotent `spawn_refresh_task` (#7083): the manager is shared by the
//! runner, ACP and daemon entry points, and a second call must neither panic, nor steal the event
//! queue from the running task, nor stop `tools/list_changed` events from reaching subscribers.
//!
//! Register in `manager/mod.rs` with `#[cfg(test)] mod refresh_task_idempotency_tests;`.

use std::time::Duration;

use super::*;
use crate::tool::{McpTool, ToolSecurityMeta};

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

#[tokio::test]
async fn spawn_refresh_task_called_twice_keeps_delivering_tool_changes() {
    let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
    mgr.spawn_refresh_task(None);
    mgr.spawn_refresh_task(None);
    mgr.inject_server_trust_for_test("srv1", McpTrustLevel::Trusted)
        .await;

    let mut rx = mgr.subscribe_tool_changes();
    mgr.clone_refresh_tx()
        .expect("refresh sender is alive")
        .try_send(crate::client::ToolRefreshEvent {
            server_id: "srv1".into(),
            tools: vec![tool("srv1", "tool_a")],
        })
        .expect("queue accepts the event");

    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .expect("a refresh must be delivered after a repeated spawn")
        .expect("watch sender alive");
    assert_eq!(rx.borrow().len(), 1);
}
