use std::sync::Arc;

use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput, extract_fenced_blocks};

use crate::manager::McpManager;

#[derive(Debug, Clone)]
pub struct McpToolExecutor {
    manager: Arc<McpManager>,
}

impl McpToolExecutor {
    #[must_use]
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

impl ToolExecutor for McpToolExecutor {
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
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let executor = McpToolExecutor::new(mgr);
        let dbg = format!("{executor:?}");
        assert!(dbg.contains("McpToolExecutor"));
    }

    #[tokio::test]
    async fn execute_no_blocks_returns_none() {
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let executor = McpToolExecutor::new(mgr);
        let result = executor.execute("no mcp blocks here").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_invalid_json_block_returns_error() {
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let executor = McpToolExecutor::new(mgr);
        let text = "```mcp\nnot json\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_valid_block_server_not_connected() {
        let mgr = Arc::new(McpManager::new(vec![], vec![]));
        let executor = McpToolExecutor::new(mgr);
        let text = "```mcp\n{\"server\":\"missing\",\"tool\":\"t\"}\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
    }
}
