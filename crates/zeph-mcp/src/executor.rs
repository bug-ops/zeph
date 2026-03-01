// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::{Arc, RwLock};

use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, extract_fenced_blocks};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::manager::McpManager;
use crate::tool::McpTool;

#[derive(Debug, Clone)]
pub struct McpToolExecutor {
    manager: Arc<McpManager>,
    tools: Arc<RwLock<Vec<McpTool>>>,
}

impl McpToolExecutor {
    #[must_use]
    pub fn new(manager: Arc<McpManager>, tools: Arc<RwLock<Vec<McpTool>>>) -> Self {
        Self { manager, tools }
    }

    pub fn set_tools(&self, tools: Vec<McpTool>) {
        let mut guard = self
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = tools;
    }
}

impl ToolExecutor for McpToolExecutor {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        let tools = self
            .tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tools
            .iter()
            .map(|t| ToolDef {
                id: t.qualified_name().into(),
                description: t.description.clone().into(),
                schema: serde_json::from_value(t.input_schema.clone())
                    .unwrap_or_else(|_| schemars::Schema::default()),
                invocation: InvocationHint::ToolCall,
            })
            .collect()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let Some((server_id, tool_name)) = call.tool_id.split_once(':') else {
            return Ok(None);
        };

        let is_known = {
            let tools = self
                .tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tools
                .iter()
                .any(|t| t.server_id == server_id && t.name == tool_name)
        };
        if !is_known {
            return Ok(None);
        }

        let args = serde_json::Value::Object(call.params.clone());
        let result = self
            .manager
            .call_tool(server_id, tool_name, args)
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

        let text = result
            .content
            .iter()
            .filter_map(|c| {
                if let rmcp::model::RawContent::Text(t) = &c.raw {
                    Some(t.text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary: text,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        }))
    }

    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let blocks = extract_fenced_blocks(response, "mcp");
        if blocks.is_empty() {
            return Ok(None);
        }

        let mut outputs = Vec::with_capacity(blocks.len());
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let instruction: McpInstruction =
                serde_json::from_str(block).map_err(|e: serde_json::Error| {
                    ToolError::Execution(std::io::Error::other(e.to_string()))
                })?;

            let result = self
                .manager
                .call_tool(&instruction.server, &instruction.tool, instruction.args)
                .await
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

            let text = result
                .content
                .iter()
                .filter_map(|c| {
                    if let rmcp::model::RawContent::Text(t) = &c.raw {
                        Some(t.text.as_str())
                    } else {
                        tracing::debug!(
                            server = instruction.server,
                            tool = instruction.tool,
                            "skipping non-text content from MCP tool"
                        );
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            outputs.push(format!(
                "[mcp:{}:{}]\n{}",
                instruction.server, instruction.tool, text,
            ));
        }

        Ok(Some(ToolOutput {
            tool_name: "mcp".to_owned(),
            summary: outputs.join("\n\n"),
            blocks_executed,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        }))
    }
}

#[derive(serde::Deserialize)]
struct McpInstruction {
    server: String,
    tool: String,
    #[serde(default = "default_args")]
    args: serde_json::Value,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_executor() -> McpToolExecutor {
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let tools = Arc::new(RwLock::new(vec![]));
        McpToolExecutor::new(mgr, tools)
    }

    #[test]
    fn parse_instruction_full() {
        let json = r#"{"server": "github", "tool": "create_issue", "args": {"title": "bug"}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "github");
        assert_eq!(instr.tool, "create_issue");
        assert_eq!(instr.args["title"], "bug");
    }

    #[test]
    fn parse_instruction_no_args() {
        let json = r#"{"server": "fs", "tool": "list_dir"}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "fs");
        assert_eq!(instr.tool, "list_dir");
        assert!(instr.args.is_object());
    }

    #[test]
    fn parse_instruction_empty_args() {
        let json = r#"{"server": "s", "tool": "t", "args": {}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert!(instr.args.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_instruction_missing_server_fails() {
        let json = r#"{"tool": "t"}"#;
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn parse_instruction_missing_tool_fails() {
        let json = r#"{"server": "s"}"#;
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn extract_mcp_blocks() {
        let text = "Here:\n```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```\nDone";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("\"server\""));
    }

    #[test]
    fn no_mcp_blocks() {
        let text = "```bash\necho hello\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert!(blocks.is_empty());
    }

    #[test]
    fn multiple_mcp_blocks() {
        let text = "```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```\n\
                    text\n\
                    ```mcp\n{\"server\":\"c\",\"tool\":\"d\"}\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parse_instruction_invalid_json() {
        let json = r#"{not valid json}"#;
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn parse_instruction_extra_fields_ignored() {
        let json = r#"{"server":"s","tool":"t","args":{},"extra":"ignored"}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "s");
        assert_eq!(instr.tool, "t");
    }

    #[test]
    fn parse_instruction_args_array() {
        let json = r#"{"server":"s","tool":"t","args":["a","b"]}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert!(instr.args.is_array());
    }

    #[test]
    fn parse_instruction_args_nested() {
        let json = r#"{"server":"s","tool":"t","args":{"nested":{"key":"val"}}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.args["nested"]["key"], "val");
    }

    #[test]
    fn default_args_is_empty_object() {
        let val = default_args();
        assert!(val.is_object());
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn extract_mcp_blocks_empty_input() {
        let blocks = extract_fenced_blocks("", "mcp");
        assert!(blocks.is_empty());
    }

    #[test]
    fn extract_mcp_blocks_other_lang_ignored() {
        let text =
            "```json\n{\"key\":\"val\"}\n```\n```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("\"server\""));
    }

    #[test]
    fn executor_construction() {
        let executor = make_executor();
        let dbg = format!("{executor:?}");
        assert!(dbg.contains("McpToolExecutor"));
    }

    #[test]
    fn tool_definitions_empty_when_no_tools() {
        let executor = make_executor();
        assert!(executor.tool_definitions().is_empty());
    }

    #[test]
    fn tool_definitions_returns_qualified_names() {
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let tools = Arc::new(RwLock::new(vec![McpTool {
            server_id: "gh".into(),
            name: "create_issue".into(),
            description: "Create a GitHub issue".into(),
            input_schema: serde_json::json!({}),
        }]));
        let executor = McpToolExecutor::new(mgr, tools);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "gh:create_issue");
        assert_eq!(defs[0].description.as_ref(), "Create a GitHub issue");
    }

    #[test]
    fn set_tools_updates_definitions() {
        let executor = make_executor();
        assert!(executor.tool_definitions().is_empty());
        executor.set_tools(vec![McpTool {
            server_id: "fs".into(),
            name: "list_dir".into(),
            description: "List directory".into(),
            input_schema: serde_json::json!({}),
        }]);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "fs:list_dir");
    }

    #[tokio::test]
    async fn execute_no_blocks_returns_none() {
        let executor = make_executor();
        let result = executor.execute("no mcp blocks here").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_invalid_json_block_returns_error() {
        let executor = make_executor();
        let text = "```mcp\nnot json\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_valid_block_server_not_connected() {
        let executor = make_executor();
        let text = "```mcp\n{\"server\":\"missing\",\"tool\":\"t\"}\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_format_returns_none() {
        let executor = make_executor();
        let call = ToolCall {
            tool_id: "no_colon_here".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_server_returns_none() {
        let executor = make_executor();
        let call = ToolCall {
            tool_id: "unknown_server:tool".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }
}
