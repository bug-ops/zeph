// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use parking_lot::RwLock;

use zeph_common::ToolName;
use zeph_common::trust_level::ToolProvenance;
use zeph_common::types::McpServerId;

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationHint {
    /// Tool invoked via ```{tag}\n...\n``` fenced block in LLM response
    FencedBlock(&'static str),
    /// Tool invoked via structured `ToolCall` JSON
    ToolCall,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub id: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub schema: schemars::Schema,
    pub invocation: InvocationHint,
    /// Raw output schema from an MCP server, if present.
    ///
    /// DO NOT convert to `schemars::Schema` — lossy; see #2931 critique P0-1.
    pub output_schema: Option<serde_json::Value>,
    /// ID of the MCP server that registered this tool, or `None` for built-in tools.
    ///
    /// This is the authoritative way to determine whether a tool originates from an MCP
    /// server — `id` is a sanitized, server-namespaced string with no reliable prefix to
    /// pattern-match on (see #5712).
    pub server_id: Option<McpServerId>,
}

impl ToolDef {
    /// Returns `true` if this tool was registered by an MCP server.
    #[must_use]
    pub fn is_mcp_tool(&self) -> bool {
        self.server_id.is_some()
    }
}

/// Registry of MCP dispatch ids, the authority for classifying a tool call's provenance.
///
/// Maps each sanitized MCP tool id (the id the model sees and the executor dispatches on) to the
/// id of the serving MCP server. Provenance is looked up here, never derived from the shape of
/// the tool name. An id known to the registry is treated as MCP; the first server registered for
/// an id wins, matching dispatch.
///
/// # Examples
///
/// ```
/// use zeph_common::trust_level::ToolProvenance;
/// use zeph_common::types::McpServerId;
/// use zeph_tools::McpToolIds;
///
/// let mut ids = McpToolIds::default();
/// ids.insert("github_create_issue".into(), "github".into());
/// assert_eq!(ids.provenance("github_create_issue"), ToolProvenance::Mcp);
/// assert_eq!(ids.provenance("bash"), ToolProvenance::Native);
/// assert_eq!(ids.server_id("github_create_issue").map(McpServerId::as_str), Some("github"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct McpToolIds(HashMap<ToolName, McpServerId>);

impl McpToolIds {
    /// Build the registry from tool definitions, keeping only MCP-served tools.
    #[must_use]
    pub fn from_definitions(defs: &[ToolDef]) -> Self {
        let mut ids = Self::default();
        for def in defs {
            if let Some(server) = &def.server_id {
                ids.insert(ToolName::new(def.id.as_ref()), server.clone());
            }
        }
        ids
    }

    /// Register an MCP tool id with its serving server.
    ///
    /// The first server registered for an id wins, matching dispatch (the first registered tool
    /// serves the id); a later registration never replaces it.
    pub fn insert(&mut self, id: ToolName, server_id: McpServerId) {
        self.0.entry(id).or_insert(server_id);
    }

    /// Union of `self` and `other`; on a shared id `self`'s server wins.
    #[must_use]
    pub fn join(&self, other: &Self) -> Self {
        let mut joined = self.clone();
        for (id, server) in &other.0 {
            joined.insert(id.clone(), server.clone());
        }
        joined
    }

    /// Returns `true` when `id` is a registered MCP dispatch id.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }

    /// Provenance of a call to the tool `id`: [`ToolProvenance::Mcp`] when registered.
    #[must_use]
    pub fn provenance(&self, id: &str) -> ToolProvenance {
        if self.contains(id) {
            ToolProvenance::Mcp
        } else {
            ToolProvenance::Native
        }
    }

    /// Serving MCP server id for `id`, when registered.
    #[must_use]
    pub fn server_id(&self, id: &str) -> Option<&McpServerId> {
        self.0.get(id)
    }
}

/// Read-only, cheaply cloneable view of the live [`McpToolIds`] published by one writer.
///
/// Consumers (trust gate, shadow sentinel, policy gate, the inline and sub-agent loops) share
/// one handle instead of keeping private copies that can drift. There is deliberately no
/// `Default` and no public constructor other than [`McpToolIdsHandle::with_writer`]: a
/// `Some(handle)` therefore always has a live [`McpToolIdsWriter`] behind it, and an empty
/// stand-in cannot make a consumer skip its fail-safe fallback.
///
/// # Examples
///
/// ```
/// use zeph_tools::{McpToolIds, McpToolIdsHandle};
///
/// let (writer, handle) = McpToolIdsHandle::with_writer();
/// assert!(!handle.contains("github_create_issue"));
///
/// let mut ids = McpToolIds::default();
/// ids.insert("github_create_issue".into(), "github".into());
/// writer.publish(ids);
/// assert!(handle.contains("github_create_issue"));
/// ```
#[derive(Debug, Clone)]
pub struct McpToolIdsHandle(Arc<RwLock<Arc<McpToolIds>>>);

impl McpToolIdsHandle {
    /// Create an empty registry together with the only writer that can publish to it.
    #[must_use]
    pub fn with_writer() -> (McpToolIdsWriter, Self) {
        let handle = Self(Arc::new(RwLock::new(Arc::new(McpToolIds::default()))));
        (McpToolIdsWriter(handle.clone()), handle)
    }

    /// The currently published registry. Cheap: an `Arc` clone.
    #[must_use]
    pub fn snapshot(&self) -> Arc<McpToolIds> {
        Arc::clone(&self.0.read())
    }

    /// Returns `true` when `id` is a registered MCP dispatch id in the current registry.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.0.read().contains(id)
    }
}

/// The single write path of an [`McpToolIdsHandle`].
///
/// Not `Clone`, so one registry has exactly one writer.
#[derive(Debug)]
pub struct McpToolIdsWriter(McpToolIdsHandle);

impl McpToolIdsWriter {
    /// Replace the published registry.
    pub fn publish(&self, ids: McpToolIds) {
        *self.0.0.write() = Arc::new(ids);
    }

    /// A read handle onto the registry this writer publishes.
    #[must_use]
    pub fn handle(&self) -> McpToolIdsHandle {
        self.0.clone()
    }
}

#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<ToolDef>,
}

impl ToolRegistry {
    #[must_use]
    pub fn from_definitions(tools: Vec<ToolDef>) -> Self {
        Self { tools }
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolDef] {
        &self.tools
    }

    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.id.as_ref() == id)
    }

    /// Format tools for prompt, excluding tools fully denied by policy.
    #[must_use]
    pub fn format_for_prompt_filtered(
        &self,
        policy: &crate::permissions::PermissionPolicy,
    ) -> String {
        let mut out = String::from("<tools>\n");
        for tool in &self.tools {
            if policy.is_fully_denied(&tool.id) {
                continue;
            }
            format_tool(&mut out, tool);
        }
        out.push_str("</tools>");
        out
    }
}

fn format_tool(out: &mut String, tool: &ToolDef) {
    let _ = writeln!(out, "## {}", tool.id);
    let _ = writeln!(out, "{}", tool.description);
    match tool.invocation {
        InvocationHint::FencedBlock(tag) => {
            let _ = writeln!(out, "Invocation: use ```{tag} fenced block");
        }
        InvocationHint::ToolCall => {
            let _ = writeln!(
                out,
                "Invocation: use tool_call with {{\"tool_id\": \"{}\", \"params\": {{...}}}}",
                tool.id
            );
        }
    }
    format_schema_params(out, &tool.schema);
    out.push('\n');
}

/// Extract the primary type when schemars renders `Option<T>` as `"type": ["T", "null"]`
/// or `"anyOf": [{"type": "T"}, {"type": "null"}]`.
fn extract_non_null_type(obj: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    if let Some(arr) = obj.get("type").and_then(|v| v.as_array()) {
        return arr.iter().filter_map(|v| v.as_str()).find(|t| *t != "null");
    }
    obj.get("anyOf")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_object())
        .filter_map(|o| o.get("type")?.as_str())
        .find(|t| *t != "null")
}

fn format_schema_params(out: &mut String, schema: &schemars::Schema) {
    let Some(obj) = schema.as_object() else {
        return;
    };
    let Some(serde_json::Value::Object(props)) = obj.get("properties") else {
        return;
    };
    if props.is_empty() {
        return;
    }

    let required: Vec<&str> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let _ = writeln!(out, "Parameters:");
    for (name, prop) in props {
        let prop_obj = prop.as_object();
        let ty = prop_obj
            .and_then(|o| {
                o.get("type")
                    .and_then(|v| v.as_str())
                    .or_else(|| extract_non_null_type(o))
            })
            .unwrap_or("string");
        let desc = prop_obj
            .and_then(|o| o.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let req = if required.contains(&name.as_str()) {
            "required"
        } else {
            "optional"
        };
        let _ = writeln!(out, "  - {name}: {desc} ({ty}, {req})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_common::trust_level::ToolProvenance;
    fn server_of(ids: &McpToolIds, id: &str) -> Option<String> {
        ids.server_id(id).map(|s| s.as_str().to_owned())
    }

    #[test]
    fn mcp_tool_ids_from_definitions_skips_native_tools() {
        let def = |id: &'static str, server: Option<&str>| ToolDef {
            id: id.into(),
            description: "".into(),
            schema: schemars::schema_for!(String),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: server.map(McpServerId::from),
        };
        let ids =
            McpToolIds::from_definitions(&[def("bash", None), def("gh_create_issue", Some("gh"))]);
        assert_eq!(ids.provenance("bash"), ToolProvenance::Native);
        assert_eq!(server_of(&ids, "gh_create_issue").as_deref(), Some("gh"));
    }

    #[test]
    fn mcp_tool_ids_first_known_server_wins() {
        let mut ids = McpToolIds::default();
        ids.insert("shared_tool".into(), "first".into());
        ids.insert("shared_tool".into(), "second".into());
        assert_eq!(server_of(&ids, "shared_tool").as_deref(), Some("first"));
    }

    #[test]
    fn mcp_tool_ids_join_is_a_union_where_self_wins() {
        let mut a = McpToolIds::default();
        a.insert("shared".into(), "a".into());
        let mut b = McpToolIds::default();
        b.insert("shared".into(), "b".into());
        b.insert("only_b".into(), "b".into());
        let joined = a.join(&b);
        assert_eq!(server_of(&joined, "shared").as_deref(), Some("a"));
        assert_eq!(server_of(&joined, "only_b").as_deref(), Some("b"));
    }

    #[test]
    fn handle_sees_every_publish_of_its_single_writer() {
        let (writer, handle) = McpToolIdsHandle::with_writer();
        let reader = handle.clone();
        assert!(!reader.contains("t"));
        let mut ids = McpToolIds::default();
        ids.insert("t".into(), "s".into());
        writer.publish(ids);
        assert!(reader.contains("t"));
        assert_eq!(server_of(&reader.snapshot(), "t").as_deref(), Some("s"));
        writer.publish(McpToolIds::default());
        assert!(!handle.contains("t"));
    }

    #[test]
    fn snapshot_is_unaffected_by_a_later_publish() {
        let (writer, handle) = McpToolIdsHandle::with_writer();
        let mut ids = McpToolIds::default();
        ids.insert("t".into(), "s".into());
        writer.publish(ids);
        let before = handle.snapshot();
        writer.publish(McpToolIds::default());
        assert!(before.contains("t"));
    }

    use crate::file::ReadParams;
    use crate::shell::BashParams;

    fn sample_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "bash".into(),
                description: "Execute a shell command".into(),
                schema: schemars::schema_for!(BashParams),
                invocation: InvocationHint::FencedBlock("bash"),
                output_schema: None,
                server_id: None,
            },
            ToolDef {
                id: "read".into(),
                description: "Read file contents".into(),
                schema: schemars::schema_for!(ReadParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            },
        ]
    }

    #[test]
    fn from_definitions_stores_tools() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        assert_eq!(reg.tools().len(), 2);
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ToolRegistry::default();
        assert!(reg.tools().is_empty());
    }

    #[test]
    fn find_existing_tool() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        assert!(reg.find("bash").is_some());
        assert!(reg.find("read").is_some());
    }

    #[test]
    fn find_nonexistent_returns_none() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        assert!(reg.find("nonexistent").is_none());
    }

    #[test]
    fn format_for_prompt_contains_tools() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt =
            reg.format_for_prompt_filtered(&crate::permissions::PermissionPolicy::default());
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("</tools>"));
        assert!(prompt.contains("## bash"));
        assert!(prompt.contains("## read"));
    }

    #[test]
    fn format_for_prompt_shows_invocation_fenced() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt =
            reg.format_for_prompt_filtered(&crate::permissions::PermissionPolicy::default());
        assert!(prompt.contains("Invocation: use ```bash fenced block"));
    }

    #[test]
    fn format_for_prompt_shows_invocation_tool_call() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt =
            reg.format_for_prompt_filtered(&crate::permissions::PermissionPolicy::default());
        assert!(prompt.contains("Invocation: use tool_call"));
        assert!(prompt.contains("\"tool_id\": \"read\""));
    }

    #[test]
    fn format_for_prompt_shows_param_info() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt =
            reg.format_for_prompt_filtered(&crate::permissions::PermissionPolicy::default());
        assert!(prompt.contains("command:"));
        assert!(prompt.contains("required"));
        assert!(prompt.contains("string"));
    }

    #[test]
    fn format_for_prompt_shows_optional_params() {
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt =
            reg.format_for_prompt_filtered(&crate::permissions::PermissionPolicy::default());
        assert!(prompt.contains("offset:"));
        assert!(prompt.contains("optional"));
        assert!(
            prompt.contains("(integer, optional)"),
            "Option<u32> should render as integer, not string: {prompt}"
        );
    }

    #[test]
    fn format_filtered_excludes_fully_denied() {
        use crate::permissions::{PermissionAction, PermissionPolicy, PermissionRule};
        use std::collections::HashMap;
        let mut rules = HashMap::new();
        rules.insert(
            "bash".to_owned(),
            vec![PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Deny,
            }],
        );
        let policy = PermissionPolicy::new(rules);
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt = reg.format_for_prompt_filtered(&policy);
        assert!(!prompt.contains("## bash"));
        assert!(prompt.contains("## read"));
    }

    #[test]
    fn format_filtered_includes_mixed_rules() {
        use crate::permissions::{PermissionAction, PermissionPolicy, PermissionRule};
        use std::collections::HashMap;
        let mut rules = HashMap::new();
        rules.insert(
            "bash".to_owned(),
            vec![
                PermissionRule {
                    pattern: "echo *".to_owned(),
                    action: PermissionAction::Allow,
                },
                PermissionRule {
                    pattern: "*".to_owned(),
                    action: PermissionAction::Deny,
                },
            ],
        );
        let policy = PermissionPolicy::new(rules);
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt = reg.format_for_prompt_filtered(&policy);
        assert!(prompt.contains("## bash"));
    }

    #[test]
    fn format_filtered_no_rules_includes_all() {
        let policy = crate::permissions::PermissionPolicy::default();
        let reg = ToolRegistry::from_definitions(sample_tools());
        let prompt = reg.format_for_prompt_filtered(&policy);
        assert!(prompt.contains("## bash"));
        assert!(prompt.contains("## read"));
    }
}
