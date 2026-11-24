// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7172: the loaded MCP tool names live in `MetricsSnapshot::mcp_tools`
//! and the tool count is derived from them, so the two can never disagree.

use zeph_core::metrics::MetricsSnapshot;

#[test]
fn test_mcp_tool_count_is_derived_from_mcp_tools() {
    let mut snapshot = MetricsSnapshot::default();
    assert!(snapshot.mcp_tools.is_empty());
    assert_eq!(snapshot.mcp_tool_count(), 0);

    snapshot.mcp_tools = vec!["github_issues".to_owned(), "github_prs".to_owned()].into();
    assert_eq!(snapshot.mcp_tool_count(), 2);
}
