// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7080: `mcp_dispatch_ids` on the executor traits and
//! `McpToolIds::join_arc`.

use std::sync::Arc;

use zeph_common::types::McpServerId;
use zeph_tools::registry::{InvocationHint, ToolDef};
use zeph_tools::{
    CompositeExecutor, DynExecutor, ErasedToolExecutor, McpToolIds, ToolError, ToolExecutor,
    ToolOutput,
};

fn def(id: &'static str, server: Option<&str>) -> ToolDef {
    ToolDef {
        id: id.into(),
        description: "".into(),
        schema: schemars::schema_for!(String),
        invocation: InvocationHint::ToolCall,
        output_schema: None,
        server_id: server.map(McpServerId::from),
    }
}

fn ids(pairs: &[(&str, &str)]) -> Arc<McpToolIds> {
    let mut ids = McpToolIds::default();
    for (id, server) in pairs {
        ids.insert((*id).into(), McpServerId::from(*server));
    }
    Arc::new(ids)
}

/// Leaf executor that relies on the trait's default `mcp_dispatch_ids`.
struct DefaultOnly(Vec<ToolDef>);

impl ToolExecutor for DefaultOnly {
    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.0.clone()
    }

    zeph_tools::tool_executor_no_inner_defaults!();
}

/// Leaf executor that overrides `mcp_dispatch_ids` with a cached `Arc`.
struct Cached {
    defs: Vec<ToolDef>,
    ids: Arc<McpToolIds>,
}

impl ToolExecutor for Cached {
    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    fn mcp_dispatch_ids(&self) -> Arc<McpToolIds> {
        Arc::clone(&self.ids)
    }

    zeph_tools::tool_executor_no_inner_defaults!();
}

#[test]
fn mcp_dispatch_ids_default_classifies_from_definitions() {
    let exec = DefaultOnly(vec![def("bash", None), def("gh_create_issue", Some("gh"))]);
    let got = exec.mcp_dispatch_ids();
    assert!(got.contains("gh_create_issue"));
    assert!(!got.contains("bash"));
    assert_eq!(
        got.server_id("gh_create_issue").map(McpServerId::as_str),
        Some("gh")
    );
}

#[test]
fn mcp_dispatch_ids_erased_default_equals_from_definitions() {
    let exec = DefaultOnly(vec![def("bash", None), def("gh_create_issue", Some("gh"))]);
    let erased: &dyn ErasedToolExecutor = &exec;
    let expected = McpToolIds::from_definitions(&erased.tool_definitions_erased());
    let got = erased.mcp_dispatch_ids_erased();
    assert!(got.contains("gh_create_issue"));
    assert!(!got.contains("bash"));
    assert_eq!(
        got.server_id("gh_create_issue"),
        expected.server_id("gh_create_issue")
    );
}

#[test]
fn mcp_dispatch_ids_override_is_returned_without_cloning_the_map() {
    let cached = ids(&[("gh_create_issue", "gh")]);
    let exec = Cached {
        defs: Vec::new(),
        ids: Arc::clone(&cached),
    };
    assert!(Arc::ptr_eq(&exec.mcp_dispatch_ids(), &cached));
}

#[test]
fn mcp_dispatch_ids_erased_composite_native_dyn_mcp_is_pointer_equal_to_cached() {
    let cached = ids(&[("gh_create_issue", "gh"), ("fs_read", "fs")]);
    let mcp = Cached {
        defs: vec![
            def("gh_create_issue", Some("gh")),
            def("fs_read", Some("fs")),
        ],
        ids: Arc::clone(&cached),
    };
    let native = DefaultOnly(vec![def("bash", None)]);
    let chain = CompositeExecutor::new(native, DynExecutor(Arc::new(mcp)));

    let erased: &dyn ErasedToolExecutor = &chain;
    let got = erased.mcp_dispatch_ids_erased();

    assert!(
        Arc::ptr_eq(&got, &cached),
        "no map clone along Composite -> Dyn -> executor"
    );
    assert!(!got.contains("bash"));
}

#[test]
fn mcp_dispatch_ids_erased_composite_equals_from_definitions_of_the_chain() {
    let mcp = DefaultOnly(vec![def("gh_create_issue", Some("gh"))]);
    let native = DefaultOnly(vec![def("bash", None)]);
    let chain = CompositeExecutor::new(native, DynExecutor(Arc::new(mcp)));
    let erased: &dyn ErasedToolExecutor = &chain;

    let expected = McpToolIds::from_definitions(&erased.tool_definitions_erased());
    let got = erased.mcp_dispatch_ids_erased();

    for id in ["gh_create_issue", "bash"] {
        assert_eq!(got.contains(id), expected.contains(id), "{id}");
    }
}

#[test]
fn join_arc_returns_a_for_empty_b() {
    let a = ids(&[("x", "s1")]);
    let b = Arc::new(McpToolIds::default());
    assert!(Arc::ptr_eq(&McpToolIds::join_arc(&a, &b), &a));
}

#[test]
fn join_arc_returns_a_for_ptr_equal_b() {
    let a = ids(&[("x", "s1")]);
    assert!(Arc::ptr_eq(&McpToolIds::join_arc(&a, &Arc::clone(&a)), &a));
}

#[test]
fn join_arc_returns_a_when_b_keys_are_a_subset() {
    let a = ids(&[("x", "s1"), ("y", "s2")]);
    let b = ids(&[("y", "other")]);
    let joined = McpToolIds::join_arc(&a, &b);
    assert!(Arc::ptr_eq(&joined, &a));
    assert_eq!(joined.server_id("y").map(McpServerId::as_str), Some("s2"));
}

#[test]
fn join_arc_returns_b_when_a_is_empty() {
    let a = Arc::new(McpToolIds::default());
    let b = ids(&[("x", "s1")]);
    assert!(Arc::ptr_eq(&McpToolIds::join_arc(&a, &b), &b));
}

#[test]
fn join_arc_builds_union_with_a_winning_on_shared_ids() {
    let a = ids(&[("x", "a-server"), ("shared", "a-server")]);
    let b = ids(&[("y", "b-server"), ("shared", "b-server")]);
    let joined = McpToolIds::join_arc(&a, &b);
    assert!(joined.contains("x") && joined.contains("y"));
    assert_eq!(
        joined.server_id("shared").map(McpServerId::as_str),
        Some("a-server")
    );
    assert_eq!(
        joined.server_id("y").map(McpServerId::as_str),
        Some("b-server")
    );
}
