// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7182: `MetricsSnapshot::mcp_tools` is a shared `Arc<[String]>`, so
//! cloning a snapshot never deep-copies the tool list.

use std::sync::Arc;

use zeph_core::metrics::MetricsSnapshot;

#[test]
fn test_mcp_tools_snapshot_clone_shares_the_tool_list() {
    let mut snapshot = MetricsSnapshot::default();
    let tools: Arc<[String]> = Arc::from(vec!["gh_issues".to_owned(), "gh_prs".to_owned()]);
    snapshot.mcp_tools = Arc::clone(&tools);
    let cloned = snapshot.clone();
    assert!(Arc::ptr_eq(&snapshot.mcp_tools, &cloned.mcp_tools));
    assert_eq!(snapshot.mcp_tool_count(), 2);
}
