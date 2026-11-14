// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Single-writer MCP tool list shared by one agent and its executor.
//!
//! [`McpToolList::new`] returns a non-`Clone` [`McpToolListWriter`] and a cloneable
//! [`McpToolListReader`]. The writer owns both the list the executor may dispatch (the *exposed*
//! list) and the [`McpToolIds`] registry that classifies results, so the two cannot drift: the
//! exposed list is always a subset of the registry. The gates and loops read the registry through
//! [`McpToolIdsHandle`] (see [`McpToolListWriter::ids_handle`]); the executor holds the reader.

use std::sync::Arc;

use parking_lot::{RwLock, RwLockReadGuard};
use zeph_common::ToolName;
use zeph_tools::{McpToolIds, McpToolIdsHandle, McpToolIdsWriter};

use crate::tool::McpTool;

/// Constructor namespace for the writer/reader pair of one MCP tool list.
#[derive(Debug)]
pub struct McpToolList;

impl McpToolList {
    /// Create a tool list seeded with `tools`, together with its only writer and a reader.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_mcp::McpToolList;
    /// use zeph_mcp::tool::{McpTool, ToolSecurityMeta};
    ///
    /// let tool = McpTool {
    ///     server_id: "fs".into(),
    ///     name: "read_file".to_owned(),
    ///     description: String::new(),
    ///     input_schema: serde_json::json!({"type": "object"}),
    ///     output_schema: None,
    ///     security_meta: ToolSecurityMeta::default(),
    /// };
    /// let (writer, reader) = McpToolList::new(vec![tool]);
    /// assert_eq!(reader.read().len(), 1);
    /// assert!(writer.ids_handle().contains("fs_read_file"));
    /// ```
    #[must_use]
    #[expect(
        clippy::new_ret_no_self,
        reason = "constructs the writer/reader pair of one tool list"
    )]
    pub fn new(tools: Vec<McpTool>) -> (McpToolListWriter, McpToolListReader) {
        let (ids_writer, ids_handle) = McpToolIdsHandle::with_writer();
        ids_writer.publish(ids_of(&tools));
        let exposed = Arc::new(RwLock::new(tools));
        (
            McpToolListWriter {
                exposed: Arc::clone(&exposed),
                ids_writer,
                ids_handle,
            },
            McpToolListReader { exposed },
        )
    }
}

fn ids_of(tools: &[McpTool]) -> McpToolIds {
    let mut ids = McpToolIds::default();
    for tool in tools {
        ids.insert(ToolName::new(tool.sanitized_id()), tool.server_id.clone());
    }
    ids
}

/// The only write path of an MCP tool list. Not `Clone`.
#[derive(Debug)]
pub struct McpToolListWriter {
    exposed: Arc<RwLock<Vec<McpTool>>>,
    ids_writer: McpToolIdsWriter,
    ids_handle: McpToolIdsHandle,
}

impl McpToolListWriter {
    /// Replace the exposed list with `tools` and rebuild the id registry from the same list.
    ///
    /// The registry is published while the exposed list is write-locked, so a reader that sees a
    /// new tool in the exposed list is guaranteed to find its id in the registry.
    pub fn publish_full(&self, tools: &[McpTool]) {
        let ids = ids_of(tools);
        let mut exposed = self.exposed.write();
        self.ids_writer.publish(ids);
        exposed.clear();
        exposed.extend_from_slice(tools);
    }

    /// Replace only the exposed list with a pruned subset of the last published full list.
    ///
    /// The id registry keeps covering the full list, so classification of any tool the executor
    /// can still dispatch is unaffected by pruning.
    pub fn publish_pruned(&self, pruned: Vec<McpTool>) {
        debug_assert!(
            pruned
                .iter()
                .all(|p| self.ids_handle.contains(&p.sanitized_id())),
            "pruned set must be a subset of the published full list"
        );
        *self.exposed.write() = pruned;
    }

    /// A read handle onto the id registry this writer publishes.
    #[must_use]
    pub fn ids_handle(&self) -> McpToolIdsHandle {
        self.ids_handle.clone()
    }
}

/// Cloneable read access to the exposed tool list, held by the executor.
#[derive(Debug, Clone)]
pub struct McpToolListReader {
    exposed: Arc<RwLock<Vec<McpTool>>>,
}

impl McpToolListReader {
    /// Lock the exposed list for reading. Never hold the guard across an `.await`.
    pub fn read(&self) -> RwLockReadGuard<'_, Vec<McpTool>> {
        self.exposed.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSecurityMeta;

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

    #[test]
    fn seed_list_is_visible_to_reader_and_registry() {
        let (writer, reader) = McpToolList::new(vec![tool("a", "x")]);
        assert_eq!(reader.read().len(), 1);
        assert!(writer.ids_handle().contains("a_x"));
    }

    #[test]
    fn publish_full_updates_exposed_list_and_registry_together() {
        let (writer, reader) = McpToolList::new(vec![tool("a", "x")]);
        let ids = writer.ids_handle();
        writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
        assert_eq!(reader.read().len(), 2);
        assert!(ids.contains("b_y"));
        writer.publish_full(&[tool("b", "y")]);
        assert_eq!(reader.read().len(), 1);
        assert!(!ids.contains("a_x"));
    }

    #[test]
    fn publish_pruned_keeps_ids_of_the_full_list() {
        let (writer, reader) = McpToolList::new(Vec::new());
        let ids = writer.ids_handle();
        writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
        writer.publish_pruned(vec![tool("a", "x")]);
        assert_eq!(reader.read().len(), 1);
        assert!(ids.contains("a_x"));
        assert!(ids.contains("b_y"), "pruned tools stay classified as MCP");
    }

    #[test]
    fn cloned_readers_share_the_single_writers_list() {
        let (writer, reader) = McpToolList::new(Vec::new());
        let other = reader.clone();
        writer.publish_full(&[tool("a", "x")]);
        assert_eq!(other.read().len(), 1);
    }

    #[test]
    fn registry_records_the_serving_server() {
        let (writer, _reader) = McpToolList::new(vec![tool("srv", "t")]);
        let snapshot = writer.ids_handle().snapshot();
        assert_eq!(
            snapshot.server_id("srv_t").map(|s| s.as_str().to_owned()),
            Some("srv".to_owned())
        );
    }
}
