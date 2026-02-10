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
            summary: outputs.join("\n\n"),
            blocks_executed,
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
}
