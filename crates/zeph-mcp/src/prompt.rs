// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Formats available MCP tools as an XML prompt block for the LLM system prompt.

use std::fmt::Write;

use crate::tool::McpTool;

/// Format a slice of MCP tools as an XML `<available_tools>` block for the system prompt.
///
/// Returns an empty string when `tools` is empty. Each tool is rendered with its
/// server ID, name, description, input schema, and a fenced `mcp` invocation example
/// so the LLM knows how to call it.
///
/// # Examples
///
/// ```
/// use zeph_mcp::tool::{McpTool, ToolSecurityMeta};
/// use zeph_mcp::prompt::format_mcp_tools_prompt;
///
/// let tools = vec![McpTool {
///     server_id: "github".to_owned(),
///     name: "create_issue".to_owned(),
///     description: "Create a GitHub issue".to_owned(),
///     input_schema: serde_json::json!({"type": "object"}),
///     security_meta: ToolSecurityMeta::default(),
/// }];
///
/// let prompt = format_mcp_tools_prompt(&tools);
/// assert!(prompt.starts_with("<available_tools>"));
/// assert!(prompt.contains("server=\"github\""));
/// assert!(prompt.contains("```mcp"));
/// ```
#[must_use]
pub fn format_mcp_tools_prompt(tools: &[McpTool]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut out = String::from("<available_tools>\n");
    for tool in tools {
        let _ = writeln!(
            out,
            "  <tool server=\"{server}\" name=\"{name}\">\n\
             \x20   <description>{desc}</description>\n\
             \x20   <parameters>{schema}</parameters>\n\
             \x20   <invocation>\n\
             ```mcp\n\
             {{\"server\": \"{server}\", \"tool\": \"{name}\", \"args\": {{...}}}}\n\
             ```\n\
             \x20   </invocation>\n\
             \x20 </tool>",
            server = tool.server_id,
            name = tool.name,
            desc = tool.description,
            schema = tool.input_schema,
        );
    }
    out.push_str("</available_tools>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(server: &str, name: &str, desc: &str) -> McpTool {
        McpTool {
            server_id: server.into(),
            name: name.into(),
            description: desc.into(),
            input_schema: serde_json::json!({"type": "object"}),
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }
    }

    #[test]
    fn empty_tools_returns_empty() {
        assert!(format_mcp_tools_prompt(&[]).is_empty());
    }

    #[test]
    fn single_tool_prompt() {
        let tools = vec![make_tool("github", "create_issue", "Create issue")];
        let prompt = format_mcp_tools_prompt(&tools);
        assert!(prompt.starts_with("<available_tools>"));
        assert!(prompt.ends_with("</available_tools>"));
        assert!(prompt.contains("server=\"github\""));
        assert!(prompt.contains("name=\"create_issue\""));
        assert!(prompt.contains("<description>Create issue</description>"));
        assert!(prompt.contains("```mcp"));
        assert!(prompt.contains("\"server\": \"github\""));
    }

    #[test]
    fn multiple_tools_prompt() {
        let tools = vec![
            make_tool("github", "create_issue", "Create issue"),
            make_tool("fs", "read_file", "Read a file"),
        ];
        let prompt = format_mcp_tools_prompt(&tools);
        assert!(prompt.contains("server=\"github\""));
        assert!(prompt.contains("server=\"fs\""));
        assert!(prompt.contains("name=\"read_file\""));
    }

    #[test]
    fn prompt_contains_parameters() {
        let tools = vec![make_tool("s", "t", "d")];
        let prompt = format_mcp_tools_prompt(&tools);
        assert!(prompt.contains("<parameters>"));
        assert!(prompt.contains("\"type\":\"object\""));
    }
}
