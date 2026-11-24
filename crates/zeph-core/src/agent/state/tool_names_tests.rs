// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! #7182: the cached MCP tool-name list is rebuilt only when the tool set changes.

use std::sync::Arc;

use zeph_mcp::McpTool;

use super::McpState;

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

#[test]
fn tool_names_arc_is_shared_until_the_tool_set_changes() {
    let mut state = McpState::default();
    state.set_tools(vec![tool("b", "two"), tool("a", "one")]);

    let first = state.tool_names();
    let second = state.tool_names();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(&*first, ["a:one", "b:two"]);

    state.extend_tools(vec![tool("c", "three")]);
    let extended = state.tool_names();
    assert!(!Arc::ptr_eq(&first, &extended));
    assert_eq!(&*extended, ["a:one", "b:two", "c:three"]);

    state.retain_tools(|t| t.server_id.as_str() != "c");
    let retained = state.tool_names();
    assert!(!Arc::ptr_eq(&extended, &retained));
    assert_eq!(&*retained, ["a:one", "b:two"]);
}
