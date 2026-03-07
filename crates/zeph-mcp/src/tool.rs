// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl McpTool {
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
}
