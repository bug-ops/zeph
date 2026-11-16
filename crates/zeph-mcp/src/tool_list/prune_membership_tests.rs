// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `publish_pruned` fails closed: a tool outside the published full list is never exposed.
//!
//! The behavior must hold in release builds, so it is plain control flow, not a `debug_assert!`;
//! these tests pass identically in debug and release (`cargo test --release -p zeph-mcp`).

use super::*;
use crate::tool::ToolSecurityMeta;

fn tool(server: &str, name: &str, description: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        security_meta: ToolSecurityMeta::default(),
    }
}

#[test]
fn publish_pruned_skips_a_tool_that_is_not_in_the_published_list() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x", "")]);

    writer.publish_pruned(vec![tool("a", "x", ""), tool("evil", "y", "")]);

    let snap = reader.snapshot();
    let names: Vec<String> = snap.tools.iter().map(McpTool::qualified_name).collect();
    assert_eq!(names, ["a:x"]);
    assert!(!snap.ids.contains("evil_y"));
    assert_eq!(snap.definitions().len(), 1, "definitions follow the tools");
}

#[test]
fn publish_pruned_exposes_the_published_copy_not_the_callers() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x", "reviewed description")]);

    writer.publish_pruned(vec![tool("a", "x", "swapped description")]);

    let snap = reader.snapshot();
    assert_eq!(snap.tools[0].description, "reviewed description");
    assert_eq!(
        snap.definitions()[0].description.as_ref(),
        "reviewed description"
    );
}

#[test]
fn publish_pruned_with_only_foreign_tools_exposes_nothing() {
    let (writer, reader) = McpToolList::new(Vec::new());
    writer.publish_full(&[tool("a", "x", "")]);

    writer.publish_pruned(vec![tool("b", "x", "")]);

    assert_eq!(
        reader.snapshot().tools.as_slice(),
        [] as [crate::tool::McpTool; 0]
    );
}
