// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Construction helpers for the deprecated rmcp Roots types (SEP-2577).
//!
//! `rmcp::model::Root` and `rmcp::model::ListRootsResult` are `#[deprecated]` as of rmcp
//! 2.0.0 — the MCP Roots capability is slated for removal per
//! [SEP-2577](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2577),
//! not expected to finalize before 2026-07-28. Until then the types remain fully
//! functional and zeph continues to advertise static filesystem roots to MCP servers.
//!
//! This module is the single `#[expect(deprecated, reason = "The adapter still constructs the rmcp Roots types it advertises.")]` boundary for *constructing* these
//! values — callers elsewhere never need to name `Root`/`ListRootsResult` to build one,
//! which keeps the deprecated surface auditable from one place instead of scattered
//! `#[expect(deprecated, reason = "The adapter must name the rmcp types in these API positions.")]` attributes. Positions that must still name the deprecated types
//! directly (struct fields, function signatures, the `ClientHandler::list_roots` trait
//! return type) carry their own narrowly-scoped
//! `#[expect(deprecated, reason = "The rmcp trait signature still exposes the deprecated roots types.")]`, since the
//! deprecated path appears in rmcp's own trait signature and cannot be hidden by a wrapper.

#![expect(
    deprecated,
    reason = "The rmcp Roots types remain the protocol surface advertised by this adapter until rmcp removes them."
)]

use rmcp::model::{ListRootsResult, Root};

/// Build a filesystem root advertised to an MCP server via `roots/list`.
///
/// # Examples
///
/// ```
/// # #![expect(deprecated, reason = "The example demonstrates the rmcp roots API still exposed by this adapter.")]
/// use zeph_mcp::roots::make_root;
///
/// let root = make_root("file:///workspace", Some("workspace"));
/// assert_eq!(root.uri, "file:///workspace");
/// assert_eq!(root.name.as_deref(), Some("workspace"));
///
/// let unnamed = make_root("file:///tmp", None::<&str>);
/// assert_eq!(unnamed.name, None);
/// ```
pub fn make_root(uri: impl Into<String>, name: Option<impl Into<String>>) -> Root {
    let root = Root::new(uri);
    match name {
        Some(n) => root.with_name(n),
        None => root,
    }
}

/// Build a `roots/list` response from a list of roots.
///
/// # Examples
///
/// ```
/// # #![expect(deprecated, reason = "The example exercises the deprecated MCP roots API.")]
/// use zeph_mcp::roots::{make_list_roots, make_root};
///
/// let result = make_list_roots(vec![make_root("file:///workspace", None::<&str>)]);
/// assert_eq!(result.roots.len(), 1);
/// ```
#[must_use]
pub fn make_list_roots(roots: Vec<Root>) -> ListRootsResult {
    ListRootsResult::new(roots)
}
