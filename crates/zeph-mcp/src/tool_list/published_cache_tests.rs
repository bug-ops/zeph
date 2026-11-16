// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for the publish-time MCP tool cache (#7080, #7091).
//!
//! Registered from `tool_list.rs` with `#[cfg(test)] mod published_cache_tests;`. Assumes
//! `McpToolListReader::snapshot() -> Arc<ExposedMcpTools>` (fields `tools`, `ids: Arc<McpToolIds>`)
//! and `McpToolListWriter::expose_full()`.

use std::sync::Arc;

use zeph_tools::{ToolCall, ToolExecutor};

use super::*;
use crate::executor::McpToolExecutor;
use crate::manager::McpManager;
use crate::policy::PolicyEnforcer;
use crate::tool::ToolSecurityMeta;

fn tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: format!("{server} {name}"),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        security_meta: ToolSecurityMeta::default(),
    }
}

fn executor(reader: McpToolListReader) -> McpToolExecutor {
    let manager = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
    McpToolExecutor::new(manager, reader)
}

#[test]
fn snapshot_after_publish_full_exposes_every_tool_and_its_ids() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
    let snap = reader.snapshot();
    assert_eq!(snap.tools.len(), 2);
    assert!(snap.ids.contains("a_x") && snap.ids.contains("b_y"));
}

#[test]
fn publish_pruned_keeps_the_ids_arc_and_shrinks_the_exposed_tools() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
    let before = reader.snapshot();

    writer.publish_pruned(vec![tool("a", "x")]);
    let after = reader.snapshot();

    assert_eq!(after.tools.len(), 1);
    assert!(
        Arc::ptr_eq(&before.ids, &after.ids),
        "pruning must not rebuild ids"
    );
    assert!(
        after.ids.contains("b_y"),
        "pruned tools stay classified as MCP"
    );
}

#[test]
fn expose_full_restores_the_full_list_without_rebuilding_ids() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
    writer.publish_pruned(vec![tool("a", "x")]);
    let pruned = reader.snapshot();

    writer.expose_full();
    let full = reader.snapshot();

    assert_eq!(full.tools.len(), 2);
    assert!(
        Arc::ptr_eq(&pruned.ids, &full.ids),
        "expose_full must keep the ids Arc"
    );
}

#[test]
fn publish_full_replaces_the_ids_arc() {
    let (writer, reader) = McpToolList::new(vec![tool("a", "x")]);
    let before = reader.snapshot();
    writer.publish_full(&[tool("b", "y")]);
    let after = reader.snapshot();
    assert!(!Arc::ptr_eq(&before.ids, &after.ids));
    assert!(!after.ids.contains("a_x"));
}

#[test]
fn mcp_executor_dispatch_ids_is_the_cached_arc_of_the_snapshot() {
    let (writer, reader) = McpToolList::new(vec![tool("a", "x")]);
    let exec = executor(reader.clone());
    assert!(Arc::ptr_eq(
        &exec.mcp_dispatch_ids(),
        &reader.snapshot().ids
    ));
    writer.publish_full(&[tool("b", "y")]);
    assert!(Arc::ptr_eq(
        &exec.mcp_dispatch_ids(),
        &reader.snapshot().ids
    ));
}

#[test]
fn mcp_executor_tool_definitions_repeated_calls_return_equal_definitions() {
    let (_writer, reader) = McpToolList::new(vec![tool("a", "x"), tool("b", "y")]);
    let exec = executor(reader);
    let summarize = |defs: Vec<zeph_tools::registry::ToolDef>| {
        defs.into_iter()
            .map(|d| (d.id.to_string(), d.description.to_string(), d.server_id))
            .collect::<Vec<_>>()
    };
    let first = summarize(exec.tool_definitions());
    let second = summarize(exec.tool_definitions());
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].0, "a_x");
}

#[tokio::test]
async fn mcp_executor_dispatch_by_cached_id_finds_the_tool_and_unknown_id_does_not() {
    let (_writer, reader) = McpToolList::new(vec![tool("a", "x")]);
    let exec = executor(reader);
    let call = |id: &str| ToolCall {
        tool_id: id.into(),
        params: serde_json::Map::new(),
        caller_id: None,
        context: None,
        tool_call_id: None,
        skill_name: None,
    };
    // The server is not connected, so a found tool fails at the manager call; an unknown id is
    // not handled at all.
    assert!(exec.execute_tool_call(&call("a_x")).await.is_err());
    assert!(matches!(
        exec.execute_tool_call(&call("nope")).await,
        Ok(None)
    ));
}
