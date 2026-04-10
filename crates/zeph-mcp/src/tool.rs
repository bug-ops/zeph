// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

/// Sensitivity level of the data a tool accesses or produces.
///
/// Used by the data-flow policy ([`crate::policy::check_data_flow`]) to enforce
/// that high-sensitivity tools can only be registered on [`McpTrustLevel::Trusted`]
/// servers.  The ordering `None < Low < Medium < High` allows `max()` comparisons
/// when computing the worst-case sensitivity of a tool set.
///
/// [`McpTrustLevel::Trusted`]: crate::manager::McpTrustLevel::Trusted
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSensitivity {
    #[default]
    None,
    Low,
    Medium,
    High,
}

/// Coarse capability class for an MCP tool.
///
/// Assigned by operator config or inferred via [`infer_security_meta`].  Stored
/// inside [`ToolSecurityMeta::capabilities`] and used by the data-flow policy to
/// decide whether a tool may run on a given server trust level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    FilesystemRead,
    FilesystemWrite,
    Network,
    Shell,
    DatabaseRead,
    DatabaseWrite,
    MemoryWrite,
    ExternalApi,
}

/// A parameter path and the injection pattern that matched it.
///
/// JSON pointer format: `/properties/key/description`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlaggedParameter {
    /// JSON pointer into `input_schema` identifying the flagged value.
    pub path: String,
    /// Name of the injection pattern that matched.
    pub pattern_name: String,
}

/// Per-tool security metadata.
///
/// Assigned by operator config or inferred from tool name heuristics at registration time.
/// Stored alongside `McpTool` in the tool registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSecurityMeta {
    /// Data sensitivity of this tool's outputs.
    #[serde(default)]
    pub data_sensitivity: DataSensitivity,
    /// Capability classes this tool exercises.
    #[serde(default)]
    pub capabilities: Vec<CapabilityClass>,
    /// Parameters whose `input_schema` values matched an injection pattern.
    #[serde(default)]
    pub flagged_parameters: Vec<FlaggedParameter>,
}

/// A single MCP tool exposed by a connected server.
///
/// `McpTool` is the canonical tool representation used throughout `zeph-mcp`.
/// It is created during server connection from `rmcp` tool list responses, enriched
/// with [`ToolSecurityMeta`] (from operator config or [`infer_security_meta`]), and
/// stored in [`crate::manager::McpManager`]'s per-server tool maps.
///
/// # Qualified vs sanitized names
///
/// - [`qualified_name`](McpTool::qualified_name) — `"server_id:name"` — used for logging,
///   Qdrant keys, and quarantine routing (the `':'` character signals external MCP content).
/// - [`sanitized_id`](McpTool::sanitized_id) — `[a-zA-Z0-9_-]{1,128}` — used as the tool
///   identifier in LLM API calls, where colons are not allowed.
///
/// # Examples
///
/// ```
/// use zeph_mcp::tool::{McpTool, ToolSecurityMeta};
///
/// let tool = McpTool {
///     server_id: "github".to_owned(),
///     name: "create_issue".to_owned(),
///     description: "Create a new GitHub issue".to_owned(),
///     input_schema: serde_json::json!({"type": "object"}),
///     security_meta: ToolSecurityMeta::default(),
/// };
///
/// assert_eq!(tool.qualified_name(), "github:create_issue");
/// assert_eq!(tool.sanitized_id(), "github_create_issue");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Per-tool security metadata. Populated from config or heuristics at registration time.
    #[serde(default)]
    pub security_meta: ToolSecurityMeta,
}

/// Infer security metadata from tool name when no explicit config exists.
///
/// Uses narrow keyword matching to minimize false positives. Generic verbs
/// ("get", "list", "search") are excluded — they appear in API tools that have
/// nothing to do with filesystem access. Only explicitly filesystem-related
/// keywords trigger filesystem capabilities.
///
/// Unknown tools (no keyword match) default to `DataSensitivity::Low` with
/// empty capabilities — mild suspicion, not zero concern.
///
/// Operator config always takes precedence over heuristic inference.
#[must_use]
pub fn infer_security_meta(tool_name: &str) -> ToolSecurityMeta {
    let name = tool_name.to_lowercase();
    let mut caps = Vec::new();
    let mut sensitivity = DataSensitivity::Low;

    // Filesystem write — explicit mutation verbs + filesystem context
    if name.contains("write")
        || name.contains("delete")
        || name.contains("move")
        || name.contains("copy")
    {
        caps.push(CapabilityClass::FilesystemWrite);
        sensitivity = sensitivity.max(DataSensitivity::Medium);
    }
    // Filesystem read — only when name contains explicit filesystem keywords
    if (name.contains("read") || name.contains("cat"))
        && (name.contains("file")
            || name.contains("dir")
            || name.contains("path")
            || name.contains("folder"))
    {
        caps.push(CapabilityClass::FilesystemRead);
        // sensitivity stays Low (read-only)
    }
    // "create" + filesystem context → write; "create" alone is too generic
    if name.contains("create")
        && (name.contains("file") || name.contains("dir") || name.contains("folder"))
    {
        caps.push(CapabilityClass::FilesystemWrite);
        sensitivity = sensitivity.max(DataSensitivity::Medium);
    }
    // Shell execution — high sensitivity
    if name.contains("shell") || name.contains("bash") || name.contains("exec") {
        caps.push(CapabilityClass::Shell);
        sensitivity = sensitivity.max(DataSensitivity::High);
    }
    // Network — explicit network verbs
    if name.contains("fetch")
        || name.contains("http")
        || name.contains("request")
        || name.contains("scrape")
        || name.contains("curl")
    {
        caps.push(CapabilityClass::Network);
        sensitivity = sensitivity.max(DataSensitivity::Medium);
    }
    // Memory write — requires "memory" in name
    if name.contains("memory")
        && (name.contains("save") || name.contains("write") || name.contains("store"))
    {
        caps.push(CapabilityClass::MemoryWrite);
        sensitivity = sensitivity.max(DataSensitivity::Medium);
    }
    // Database — explicit SQL/database keywords
    if name.contains("sql") || name.contains("database") {
        caps.push(CapabilityClass::DatabaseRead);
        sensitivity = sensitivity.max(DataSensitivity::Medium);
    }

    ToolSecurityMeta {
        data_sensitivity: sensitivity,
        capabilities: caps,
        flagged_parameters: Vec::new(),
    }
}

/// Details of a `sanitized_id` collision between two registered tools.
///
/// Returned by [`detect_collisions`]. When two tools share the same [`McpTool::sanitized_id`],
/// the first-registered tool wins dispatch and the second becomes unreachable.
/// Callers should log or surface these at connect time.
///
/// Even when trust levels differ, the executor dispatches on `sanitized_id`, so a collision
/// means one tool silently shadows the other regardless of trust.
#[derive(Debug, Clone)]
pub struct ToolCollision {
    pub sanitized_id: String,
    pub server_a: String,
    pub qualified_a: String,
    pub trust_a: crate::manager::McpTrustLevel,
    pub server_b: String,
    pub qualified_b: String,
    pub trust_b: crate::manager::McpTrustLevel,
}

/// Detect `sanitized_id` collisions across a flat tool list.
///
/// Groups tools by `sanitized_id`. Returns one `ToolCollision` per pair of tools that share a
/// `sanitized_id`. The first-registered tool wins dispatch; all subsequent tools are shadowed.
///
/// The `trust_map` maps `server_id` → trust level.
#[must_use]
pub fn detect_collisions<S: std::hash::BuildHasher>(
    tools: &[McpTool],
    trust_map: &std::collections::HashMap<String, crate::manager::McpTrustLevel, S>,
) -> Vec<ToolCollision> {
    use std::collections::HashMap;

    // Map sanitized_id → first tool that claimed it.
    let mut seen: HashMap<String, &McpTool> = HashMap::new();
    let mut collisions = Vec::new();

    for tool in tools {
        let sid = tool.sanitized_id();
        if let Some(first) = seen.get(&sid) {
            let trust_a = trust_map
                .get(&first.server_id)
                .copied()
                .unwrap_or(crate::manager::McpTrustLevel::Untrusted);
            let trust_b = trust_map
                .get(&tool.server_id)
                .copied()
                .unwrap_or(crate::manager::McpTrustLevel::Untrusted);
            collisions.push(ToolCollision {
                sanitized_id: sid,
                server_a: first.server_id.clone(),
                qualified_a: first.qualified_name(),
                trust_a,
                server_b: tool.server_id.clone(),
                qualified_b: tool.qualified_name(),
                trust_b,
            });
        } else {
            seen.insert(sid, tool);
        }
    }

    collisions
}

impl McpTool {
    /// Return the fully-qualified tool name in `"server_id:name"` format.
    ///
    /// This format is used for Qdrant keys, logging, and quarantine routing.
    /// The presence of `':'` in the name is the signal used by `zeph-core`'s
    /// `sanitize_tool_output()` to classify output as external (MCP) content.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_mcp::tool::{McpTool, ToolSecurityMeta};
    ///
    /// let tool = McpTool {
    ///     server_id: "fs".to_owned(),
    ///     name: "read_file".to_owned(),
    ///     description: "Read a file".to_owned(),
    ///     input_schema: serde_json::json!({}),
    ///     security_meta: ToolSecurityMeta::default(),
    /// };
    /// assert_eq!(tool.qualified_name(), "fs:read_file");
    /// ```
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}:{}", self.server_id, self.name)
    }

    /// Returns a name safe for LLM APIs that restrict tool names to `[a-zA-Z0-9_-]{1,128}`.
    ///
    /// Replaces the colon separator and any other disallowed characters with `_`.
    /// Truncates to 128 characters if the result would exceed the API limit.
    ///
    /// **Collision note**: different `(server_id, name)` pairs can produce the same sanitized id
    /// (e.g. `"a.b:c"` and `"a:b_c"` both yield `"a_b_c"`). Callers that register multiple MCP
    /// servers should detect and warn on such collisions before dispatching.
    #[must_use]
    pub fn sanitized_id(&self) -> String {
        const MAX_LEN: usize = 128;
        let raw: String = self
            .qualified_name()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if raw.len() > MAX_LEN {
            tracing::warn!(
                server_id = %self.server_id,
                name = %self.name,
                len = raw.len(),
                "MCP tool sanitized_id exceeds 128 chars and will be truncated"
            );
            raw.chars().take(MAX_LEN).collect()
        } else {
            raw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(server: &str, name: &str) -> McpTool {
        McpTool {
            server_id: server.into(),
            name: name.into(),
            description: "test tool".into(),
            input_schema: serde_json::json!({}),
            security_meta: ToolSecurityMeta::default(),
        }
    }

    #[test]
    fn qualified_name_format() {
        let tool = make_tool("github", "create_issue");
        assert_eq!(tool.qualified_name(), "github:create_issue");
    }

    #[test]
    fn sanitized_id_replaces_colon() {
        let tool = make_tool("github", "create_issue");
        assert_eq!(tool.sanitized_id(), "github_create_issue");
    }

    #[test]
    fn sanitized_id_replaces_spaces_and_dots() {
        let tool = make_tool("my server", "tool.name");
        assert_eq!(tool.sanitized_id(), "my_server_tool_name");
    }

    #[test]
    fn sanitized_id_preserves_hyphens_and_underscores() {
        let tool = make_tool("my-server", "my_tool");
        assert_eq!(tool.sanitized_id(), "my-server_my_tool");
    }

    #[test]
    fn tool_roundtrip_json() {
        let tool = make_tool("fs", "read_file");
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: McpTool = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.server_id, "fs");
        assert_eq!(parsed.name, "read_file");
        assert_eq!(parsed.description, "test tool");
    }

    #[test]
    fn tool_clone() {
        let tool = make_tool("a", "b");
        let cloned = tool.clone();
        assert_eq!(tool.qualified_name(), cloned.qualified_name());
    }

    #[test]
    fn sanitized_id_replaces_unicode_chars() {
        let tool = make_tool("ünïcödé", "tëst");
        let id = tool.sanitized_id();
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "sanitized_id must only contain [a-zA-Z0-9_-], got: {id}"
        );
    }

    #[test]
    fn sanitized_id_preserves_numbers() {
        let tool = make_tool("server42", "tool99");
        assert_eq!(tool.sanitized_id(), "server42_tool99");
    }

    #[test]
    fn sanitized_id_at_exactly_128_chars_not_truncated() {
        // Construct server_id and name such that qualified_name is exactly 128 chars.
        // qualified_name = server_id + ":" + name, so total = server_id.len + 1 + name.len = 128.
        let server_id = "a".repeat(63);
        let name = "b".repeat(64);
        let tool = make_tool(&server_id, &name);
        let id = tool.sanitized_id();
        // The colon becomes '_', so length stays 128.
        assert_eq!(id.len(), 128);
        assert!(id.chars().all(|c| c == 'a' || c == '_' || c == 'b'));
    }

    #[test]
    fn sanitized_id_longer_than_128_chars_is_truncated() {
        // sanitized_id() truncates to 128 chars to stay within Claude API limits.
        let server_id = "a".repeat(100);
        let name = "b".repeat(100);
        let tool = make_tool(&server_id, &name);
        let id = tool.sanitized_id();
        assert_eq!(id.len(), 128);
        assert!(id.chars().all(|c| c == 'a' || c == '_' || c == 'b'));
    }

    #[test]
    fn sanitized_id_collision_two_different_tools() {
        // "a.b" + ":" + "c" and "a" + ":" + "b_c" both sanitize to "a_b_c".
        let tool_a = make_tool("a.b", "c");
        let tool_b = make_tool("a", "b_c");
        assert_eq!(tool_a.sanitized_id(), tool_b.sanitized_id());
        // The qualified names must differ — callers relying on sanitized_id for dedup
        // need to be aware that collisions are possible.
        assert_ne!(tool_a.qualified_name(), tool_b.qualified_name());
    }

    #[test]
    fn sanitized_id_all_ascii_special_chars_replaced() {
        // Verify every non-alphanumeric, non-hyphen, non-underscore ASCII char maps to '_'.
        let tool = make_tool("srv!@#$%^&*()+", "tool/\\<>");
        let id = tool.sanitized_id();
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "got unexpected chars in: {id}"
        );
    }

    #[test]
    fn tool_roundtrip_json_with_security_meta() {
        let tool = McpTool {
            server_id: "fs".into(),
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({}),
            security_meta: ToolSecurityMeta {
                data_sensitivity: DataSensitivity::Medium,
                capabilities: vec![CapabilityClass::FilesystemWrite],
                flagged_parameters: Vec::new(),
            },
        };
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: McpTool = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.security_meta.data_sensitivity,
            DataSensitivity::Medium
        );
        assert_eq!(
            parsed.security_meta.capabilities,
            vec![CapabilityClass::FilesystemWrite]
        );
    }

    #[test]
    fn tool_default_security_meta_is_none_sensitivity() {
        let tool = make_tool("srv", "some_tool");
        assert_eq!(tool.security_meta.data_sensitivity, DataSensitivity::None);
        assert!(tool.security_meta.capabilities.is_empty());
    }

    // infer_security_meta tests

    #[test]
    fn infer_shell_tools_get_high_sensitivity() {
        let meta = infer_security_meta("exec_command");
        assert_eq!(meta.data_sensitivity, DataSensitivity::High);
        assert!(meta.capabilities.contains(&CapabilityClass::Shell));
    }

    #[test]
    fn infer_bash_tool_is_shell() {
        let meta = infer_security_meta("run_bash");
        assert_eq!(meta.data_sensitivity, DataSensitivity::High);
        assert!(meta.capabilities.contains(&CapabilityClass::Shell));
    }

    #[test]
    fn infer_write_file_gets_filesystem_write_medium() {
        let meta = infer_security_meta("write_file");
        assert_eq!(meta.data_sensitivity, DataSensitivity::Medium);
        assert!(
            meta.capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_read_file_gets_filesystem_read_low() {
        let meta = infer_security_meta("read_file");
        assert_eq!(meta.data_sensitivity, DataSensitivity::Low);
        assert!(meta.capabilities.contains(&CapabilityClass::FilesystemRead));
    }

    #[test]
    fn infer_delete_gets_filesystem_write() {
        let meta = infer_security_meta("delete_file");
        assert!(
            meta.capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_create_dir_gets_filesystem_write() {
        let meta = infer_security_meta("create_dir");
        assert!(
            meta.capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_network_fetch_gets_network() {
        let meta = infer_security_meta("fetch_url");
        assert!(meta.capabilities.contains(&CapabilityClass::Network));
        assert_eq!(meta.data_sensitivity, DataSensitivity::Medium);
    }

    #[test]
    fn infer_scrape_gets_network() {
        let meta = infer_security_meta("web_scrape");
        assert!(meta.capabilities.contains(&CapabilityClass::Network));
    }

    #[test]
    fn infer_sql_query_gets_database() {
        let meta = infer_security_meta("run_sql_query");
        assert!(meta.capabilities.contains(&CapabilityClass::DatabaseRead));
    }

    #[test]
    fn infer_memory_save_gets_memory_write() {
        let meta = infer_security_meta("memory_save");
        assert!(meta.capabilities.contains(&CapabilityClass::MemoryWrite));
    }

    // No false positives on generic names
    #[test]
    fn infer_generic_get_weather_no_filesystem_caps() {
        let meta = infer_security_meta("get_weather");
        assert!(!meta.capabilities.contains(&CapabilityClass::FilesystemRead));
        assert!(
            !meta
                .capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
        assert!(!meta.capabilities.contains(&CapabilityClass::Shell));
    }

    #[test]
    fn infer_list_models_no_filesystem_caps() {
        let meta = infer_security_meta("list_models");
        assert!(!meta.capabilities.contains(&CapabilityClass::FilesystemRead));
        assert!(
            !meta
                .capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_search_docs_no_filesystem_caps() {
        let meta = infer_security_meta("search_docs");
        assert!(!meta.capabilities.contains(&CapabilityClass::FilesystemRead));
        assert!(
            !meta
                .capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_save_note_no_memory_write_without_memory_keyword() {
        // "save" alone should not trigger MemoryWrite — needs "memory" in name
        let meta = infer_security_meta("save_note");
        assert!(!meta.capabilities.contains(&CapabilityClass::MemoryWrite));
    }

    #[test]
    fn infer_unknown_tool_defaults_to_low_sensitivity_empty_caps() {
        let meta = infer_security_meta("do_something_random");
        assert_eq!(meta.data_sensitivity, DataSensitivity::Low);
        assert!(meta.capabilities.is_empty());
    }

    #[test]
    fn data_sensitivity_ordering() {
        assert!(DataSensitivity::None < DataSensitivity::Low);
        assert!(DataSensitivity::Low < DataSensitivity::Medium);
        assert!(DataSensitivity::Medium < DataSensitivity::High);
    }

    #[test]
    fn infer_http_keyword_gets_network() {
        let meta = infer_security_meta("make_http_request");
        assert!(meta.capabilities.contains(&CapabilityClass::Network));
        assert_eq!(meta.data_sensitivity, DataSensitivity::Medium);
    }

    #[test]
    fn infer_database_keyword_gets_database_read() {
        let meta = infer_security_meta("query_database");
        assert!(meta.capabilities.contains(&CapabilityClass::DatabaseRead));
    }

    #[test]
    fn infer_move_gets_filesystem_write() {
        let meta = infer_security_meta("move_file");
        assert!(
            meta.capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_copy_gets_filesystem_write() {
        let meta = infer_security_meta("copy_file");
        assert!(
            meta.capabilities
                .contains(&CapabilityClass::FilesystemWrite)
        );
    }

    #[test]
    fn infer_curl_gets_network() {
        let meta = infer_security_meta("run_curl");
        assert!(meta.capabilities.contains(&CapabilityClass::Network));
        assert_eq!(meta.data_sensitivity, DataSensitivity::Medium);
    }

    // --- detect_collisions ---

    fn trust_map(
        entries: &[(&str, crate::manager::McpTrustLevel)],
    ) -> std::collections::HashMap<String, crate::manager::McpTrustLevel> {
        entries.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn detect_collisions_no_collision_happy_path() {
        let tools = vec![
            make_tool("server_a", "tool_one"),
            make_tool("server_b", "tool_two"),
        ];
        let tm = trust_map(&[
            ("server_a", crate::manager::McpTrustLevel::Trusted),
            ("server_b", crate::manager::McpTrustLevel::Trusted),
        ]);
        let cols = detect_collisions(&tools, &tm);
        assert!(cols.is_empty(), "different sanitized_ids must not collide");
    }

    #[test]
    fn detect_collisions_different_trust_collision() {
        // "a.b:c" and "a:b_c" both sanitize to "a_b_c" — collision across trust levels.
        let tool_a = make_tool("a.b", "c");
        let tool_b = make_tool("a", "b_c");
        let tm = trust_map(&[
            ("a.b", crate::manager::McpTrustLevel::Trusted),
            ("a", crate::manager::McpTrustLevel::Untrusted),
        ]);
        let cols = detect_collisions(&[tool_a, tool_b], &tm);
        assert_eq!(cols.len(), 1);
        let col = &cols[0];
        assert_eq!(col.sanitized_id, "a_b_c");
        assert_eq!(col.server_a, "a.b");
        assert_eq!(col.server_b, "a");
        assert_eq!(col.trust_a, crate::manager::McpTrustLevel::Trusted);
        assert_eq!(col.trust_b, crate::manager::McpTrustLevel::Untrusted);
    }

    #[test]
    fn detect_collisions_same_trust_collision() {
        // Both servers are Untrusted and share a sanitized_id.
        let tool_a = make_tool("a.b", "c");
        let tool_b = make_tool("a", "b_c");
        let tm = trust_map(&[
            ("a.b", crate::manager::McpTrustLevel::Untrusted),
            ("a", crate::manager::McpTrustLevel::Untrusted),
        ]);
        let cols = detect_collisions(&[tool_a, tool_b], &tm);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].trust_a, crate::manager::McpTrustLevel::Untrusted);
        assert_eq!(cols[0].trust_b, crate::manager::McpTrustLevel::Untrusted);
    }

    #[test]
    fn detect_collisions_multiple_collisions_reported() {
        // Three tools, all sharing the same sanitized_id "srv_tool".
        let t1 = make_tool("srv", "tool");
        let t2 = make_tool("srv.x", "tool"); // "srv_x_tool" — different, no collision with t1
        let t3 = make_tool("srv", "tool"); // exact duplicate of t1 — collision
        let tm = trust_map(&[("srv", crate::manager::McpTrustLevel::Untrusted)]);
        let cols = detect_collisions(&[t1, t2, t3], &tm);
        // t1 and t3 share "srv_tool"; t2 is "srv_x_tool" — one collision
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].sanitized_id, "srv_tool");
    }

    #[test]
    fn detect_collisions_unknown_server_defaults_to_untrusted() {
        let tool_a = make_tool("a.b", "c");
        let tool_b = make_tool("a", "b_c");
        // No entries in trust_map — both should default to Untrusted.
        let cols = detect_collisions(&[tool_a, tool_b], &std::collections::HashMap::new());
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].trust_a, crate::manager::McpTrustLevel::Untrusted);
        assert_eq!(cols[0].trust_b, crate::manager::McpTrustLevel::Untrusted);
    }
}
