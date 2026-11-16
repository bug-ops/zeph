// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Single-writer MCP tool list shared by one agent and its executor.
//!
//! [`McpToolList::new`] returns a non-`Clone` [`McpToolListWriter`] and a cloneable
//! [`McpToolListReader`]. The writer owns both the list the executor may dispatch (the *exposed*
//! list) and the [`McpToolIds`] registry that classifies results, so the two cannot drift: the
//! exposed list is always a subset of the registry. The gates and loops read the registry through
//! [`McpToolIdsHandle`] (see [`McpToolListWriter::ids_handle`]); the executor holds the reader.
//!
//! Everything derived from a tool (its sanitized dispatch id and its `ToolDef`, including the
//! parsed input schema) is built once per tool-list change in [`McpToolListWriter::publish_full`]
//! and shared by `Arc`. Per-turn operations (`publish_pruned`, `expose_full`, reads) only clone
//! `Arc`s and never rebuild definitions or the id registry.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use zeph_common::ToolName;
use zeph_common::types::McpServerId;
use zeph_tools::registry::{InvocationHint, ToolDef};
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
    /// assert_eq!(reader.snapshot().tools.len(), 1);
    /// assert!(writer.ids_handle().contains("fs_read_file"));
    /// ```
    #[must_use]
    #[expect(
        clippy::new_ret_no_self,
        reason = "constructs the writer/reader pair of one tool list"
    )]
    pub fn new(tools: Vec<McpTool>) -> (McpToolListWriter, McpToolListReader) {
        let (ids_writer, ids_handle) = McpToolIdsHandle::with_writer();
        let full = FullList::build(tools);
        ids_writer.publish_shared(Arc::clone(&full.exposed.ids));
        let exposed = Arc::new(RwLock::new(Arc::clone(&full.exposed)));
        (
            McpToolListWriter {
                exposed: Arc::clone(&exposed),
                full: Mutex::new(full),
                ids_writer,
                ids_handle,
            },
            McpToolListReader { exposed },
        )
    }
}

/// A tool together with everything derived from it at publish time.
#[derive(Debug)]
struct PublishedMcpTool {
    tool: McpTool,
    definition: ToolDef,
}

impl PublishedMcpTool {
    fn build(tool: &McpTool) -> Self {
        let definition = ToolDef {
            id: tool.sanitized_id().into(),
            description: tool.description.clone().into(),
            schema: serde_json::from_value(tool.input_schema.clone())
                .unwrap_or_else(|_| schemars::Schema::default()),
            invocation: InvocationHint::ToolCall,
            output_schema: tool.output_schema.clone(),
            server_id: Some(tool.server_id.clone()),
        };
        Self {
            tool: tool.clone(),
            definition,
        }
    }
}

fn ids_of(tools: &[McpTool]) -> McpToolIds {
    let mut ids = McpToolIds::default();
    for tool in tools {
        ids.insert(ToolName::new(tool.sanitized_id()), tool.server_id.clone());
    }
    ids
}

/// Immutable view of the exposed tools together with the dispatch ids published with them.
///
/// Obtained from [`McpToolListReader::snapshot`]; holding it never blocks the writer.
#[derive(Debug)]
pub struct ExposedMcpTools {
    /// Tools the executor may dispatch.
    pub tools: Vec<McpTool>,
    /// Dispatch ids covering the full published list, shared with the registry handle.
    pub ids: Arc<McpToolIds>,
    published: Vec<Arc<PublishedMcpTool>>,
}

impl ExposedMcpTools {
    /// The cached tool definitions of the exposed tools, in list order.
    pub(crate) fn definitions(&self) -> Vec<ToolDef> {
        self.published
            .iter()
            .map(|p| p.definition.clone())
            .collect()
    }

    /// The exposed tool dispatched by the sanitized id `id`, if any (first match wins).
    pub(crate) fn find_by_dispatch_id(&self, id: &str) -> Option<&McpTool> {
        self.published
            .iter()
            .find(|p| p.definition.id == id)
            .map(|p| &p.tool)
    }
}

/// The full published list: the cached view plus a lookup used to prune without rebuilding.
#[derive(Debug)]
struct FullList {
    exposed: Arc<ExposedMcpTools>,
    by_identity: HashMap<McpServerId, HashMap<String, Arc<PublishedMcpTool>>>,
    shadowed: HashSet<(McpServerId, String)>,
}

impl FullList {
    /// Build the published view of `tools`.
    ///
    /// A tool whose sanitized id was already claimed by an earlier tool is shadowed: it stays in
    /// `shadowed` but is never exposed, so dispatch, the id registry and every audit record agree
    /// on the one server that serves an id (first registered wins).
    fn build(tools: Vec<McpTool>) -> Self {
        let mut claimed: HashMap<String, McpServerId> = HashMap::new();
        let mut published: Vec<Arc<PublishedMcpTool>> = Vec::with_capacity(tools.len());
        let mut shadowed: HashSet<(McpServerId, String)> = HashSet::new();
        for tool in tools {
            let id = tool.sanitized_id();
            if claimed.contains_key(&id) {
                shadowed.insert((tool.server_id.clone(), tool.name.clone()));
                continue;
            }
            claimed.insert(id, tool.server_id.clone());
            published.push(Arc::new(PublishedMcpTool::build(&tool)));
        }
        let mut by_identity: HashMap<McpServerId, HashMap<String, Arc<PublishedMcpTool>>> =
            HashMap::new();
        for p in &published {
            by_identity
                .entry(p.tool.server_id.clone())
                .or_default()
                .insert(p.tool.name.clone(), Arc::clone(p));
        }
        let exposed_tools: Vec<McpTool> = published.iter().map(|p| p.tool.clone()).collect();
        Self {
            exposed: Arc::new(ExposedMcpTools {
                ids: Arc::new(ids_of(&exposed_tools)),
                tools: exposed_tools,
                published,
            }),
            by_identity,
            shadowed,
        }
    }
}

/// The only write path of an MCP tool list. Not `Clone`.
#[derive(Debug)]
pub struct McpToolListWriter {
    exposed: Arc<RwLock<Arc<ExposedMcpTools>>>,
    full: Mutex<FullList>,
    ids_writer: McpToolIdsWriter,
    ids_handle: McpToolIdsHandle,
}

impl McpToolListWriter {
    /// Replace the exposed list with `tools` and rebuild the id registry and the cached
    /// definitions from the same list.
    ///
    /// The registry is published while the exposed slot is write-locked, so a reader that sees a
    /// new tool in the exposed list is guaranteed to find its id in the registry.
    pub fn publish_full(&self, tools: &[McpTool]) {
        let full = FullList::build(tools.to_vec());
        let mut exposed = self.exposed.write();
        self.ids_writer
            .publish_shared(Arc::clone(&full.exposed.ids));
        *exposed = Arc::clone(&full.exposed);
        *self.full.lock() = full;
    }

    /// Expose only `pruned`, a subset of the last published full list.
    ///
    /// The id registry keeps covering the full list, so classification of any tool the executor
    /// can still dispatch is unaffected by pruning. Cached definitions are reused by `Arc`.
    ///
    /// Fails closed: a tool of `pruned` that is not in the last published full list is skipped
    /// with a WARN, in release builds too, so the exposed list can never hold a tool the id
    /// registry would classify as native. The exposed tools are the published ones, not the
    /// caller's copies.
    pub fn publish_pruned(&self, pruned: impl IntoIterator<Item = McpTool>) {
        let full = self.full.lock();
        let ids = Arc::clone(&full.exposed.ids);
        let published: Vec<Arc<PublishedMcpTool>> = pruned
            .into_iter()
            .filter_map(|t| {
                let member = full
                    .by_identity
                    .get(&t.server_id)
                    .and_then(|by_name| by_name.get(&t.name))
                    .map(Arc::clone);
                let shadowed = full
                    .shadowed
                    .contains(&(t.server_id.clone(), t.name.clone()));
                if member.is_none() && !shadowed {
                    tracing::warn!(
                        server_id = %t.server_id,
                        tool = %t.name,
                        "pruned MCP tool is not in the published list; not exposed"
                    );
                }
                member
            })
            .collect();
        drop(full);
        let tools = published.iter().map(|p| p.tool.clone()).collect();
        *self.exposed.write() = Arc::new(ExposedMcpTools {
            tools,
            ids,
            published,
        });
    }

    /// Expose the full published list again after a prune, keeping the published ids `Arc`.
    ///
    /// An `Arc` swap: nothing is cloned or rebuilt.
    pub fn expose_full(&self) {
        let full = Arc::clone(&self.full.lock().exposed);
        *self.exposed.write() = full;
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
    exposed: Arc<RwLock<Arc<ExposedMcpTools>>>,
}

impl McpToolListReader {
    /// Cheap consistent snapshot of the exposed tools and their ids: an `Arc` clone, so no lock is
    /// held while the caller uses it (safe across `.await`).
    #[must_use]
    pub fn snapshot(&self) -> Arc<ExposedMcpTools> {
        Arc::clone(&self.exposed.read())
    }
}

#[cfg(test)]
mod prune_membership_tests;
#[cfg(test)]
mod published_cache_tests;

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
        assert_eq!(reader.snapshot().tools.len(), 1);
        assert!(writer.ids_handle().contains("a_x"));
    }

    #[test]
    fn publish_full_updates_exposed_list_and_registry_together() {
        let (writer, reader) = McpToolList::new(vec![tool("a", "x")]);
        let ids = writer.ids_handle();
        writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
        assert_eq!(reader.snapshot().tools.len(), 2);
        assert!(ids.contains("b_y"));
        writer.publish_full(&[tool("b", "y")]);
        assert_eq!(reader.snapshot().tools.len(), 1);
        assert!(!ids.contains("a_x"));
    }

    #[test]
    fn publish_pruned_keeps_ids_of_the_full_list() {
        let (writer, reader) = McpToolList::new(Vec::new());
        let ids = writer.ids_handle();
        writer.publish_full(&[tool("a", "x"), tool("b", "y")]);
        writer.publish_pruned(vec![tool("a", "x")]);
        assert_eq!(reader.snapshot().tools.len(), 1);
        assert!(ids.contains("a_x"));
        assert!(ids.contains("b_y"), "pruned tools stay classified as MCP");
    }

    #[test]
    fn shadowed_tool_is_never_exposed_and_pruning_cannot_revive_it() {
        let first = tool("a", "b_c");
        let second = tool("a_b", "c");
        assert_eq!(first.sanitized_id(), second.sanitized_id());
        let (writer, reader) = McpToolList::new(Vec::new());
        writer.publish_full(&[first.clone(), second.clone()]);

        let snapshot = reader.snapshot();
        assert_eq!(snapshot.tools, vec![first]);
        assert_eq!(
            snapshot
                .ids
                .server_id("a_b_c")
                .map(|s| s.as_str().to_owned()),
            Some("a".to_owned())
        );

        writer.publish_pruned(vec![second]);
        assert_eq!(reader.snapshot().tools, []);
        assert!(reader.snapshot().find_by_dispatch_id("a_b_c").is_none());
    }

    #[test]
    fn cloned_readers_share_the_single_writers_list() {
        let (writer, reader) = McpToolList::new(Vec::new());
        let other = reader.clone();
        writer.publish_full(&[tool("a", "x")]);
        assert_eq!(other.snapshot().tools.len(), 1);
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
