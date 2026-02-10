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
}
