use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Boolean,
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String => f.write_str("string"),
            Self::Integer => f.write_str("integer"),
            Self::Boolean => f.write_str("boolean"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParamDef {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
    pub param_type: ParamType,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub id: &'static str,
    pub description: &'static str,
    pub parameters: Vec<ParamDef>,
}

#[derive(Debug)]
pub struct ToolRegistry {
    tools: Vec<ToolDef>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: builtin_tools(),
        }
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolDef] {
        &self.tools
    }

    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// Format tools for prompt, excluding tools fully denied by policy.
    #[must_use]
    pub fn format_for_prompt_filtered(
        &self,
        policy: &crate::permissions::PermissionPolicy,
    ) -> String {
        use std::fmt::Write;
        let mut out = String::from("<tools>\n");
        for tool in &self.tools {
            if policy.is_fully_denied(tool.id) {
                continue;
            }
            let _ = writeln!(out, "## {}", tool.id);
            let _ = writeln!(out, "{}", tool.description);
            if !tool.parameters.is_empty() {
                let _ = writeln!(out, "Parameters:");
                for p in &tool.parameters {
                    let req = if p.required { "required" } else { "optional" };
                    let _ = writeln!(
                        out,
                        "  - {}: {} ({}, {})",
                        p.name, p.description, p.param_type, req
                    );
                }
            }
            out.push('\n');
        }
        out.push_str("</tools>");
        out
    }

    #[must_use]
    pub fn format_for_prompt(&self) -> String {
        use std::fmt::Write;
        let mut out = String::from("<tools>\n");
        for tool in &self.tools {
            let _ = writeln!(out, "## {}", tool.id);
            let _ = writeln!(out, "{}", tool.description);
            if !tool.parameters.is_empty() {
                let _ = writeln!(out, "Parameters:");
                for p in &tool.parameters {
                    let req = if p.required { "required" } else { "optional" };
                    let _ = writeln!(
                        out,
                        "  - {}: {} ({}, {})",
                        p.name, p.description, p.param_type, req
                    );
                }
            }
            out.push('\n');
        }
        out.push_str("</tools>");
        out
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn builtin_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            id: "bash",
            description: "Execute a shell command",
            parameters: vec![ParamDef {
                name: "command",
                description: "The bash command to execute",
                required: true,
                param_type: ParamType::String,
            }],
        },
        ToolDef {
            id: "read",
            description: "Read file contents",
            parameters: vec![
                ParamDef {
                    name: "path",
                    description: "Absolute or relative file path",
                    required: true,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "offset",
                    description: "Line number to start reading from",
                    required: false,
                    param_type: ParamType::Integer,
                },
                ParamDef {
                    name: "limit",
                    description: "Number of lines to read",
                    required: false,
                    param_type: ParamType::Integer,
                },
            ],
        },
        ToolDef {
            id: "edit",
            description: "Replace a string in a file",
            parameters: vec![
                ParamDef {
                    name: "path",
                    description: "File path to edit",
                    required: true,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "old_string",
                    description: "Text to find and replace",
                    required: true,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "new_string",
                    description: "Replacement text",
                    required: true,
                    param_type: ParamType::String,
                },
            ],
        },
        ToolDef {
            id: "write",
            description: "Write content to a file",
            parameters: vec![
                ParamDef {
                    name: "path",
                    description: "File path to write",
                    required: true,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "content",
                    description: "Content to write",
                    required: true,
                    param_type: ParamType::String,
                },
            ],
        },
        ToolDef {
            id: "glob",
            description: "Find files matching a glob pattern",
            parameters: vec![ParamDef {
                name: "pattern",
                description: "Glob pattern (e.g. **/*.rs)",
                required: true,
                param_type: ParamType::String,
            }],
        },
        ToolDef {
            id: "grep",
            description: "Search file contents with regex",
            parameters: vec![
                ParamDef {
                    name: "pattern",
                    description: "Regex pattern to search for",
                    required: true,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "path",
                    description: "Directory or file to search in",
                    required: false,
                    param_type: ParamType::String,
                },
                ParamDef {
                    name: "case_sensitive",
                    description: "Whether search is case-sensitive",
                    required: false,
                    param_type: ParamType::Boolean,
                },
            ],
        },
        ToolDef {
            id: "web_scrape",
            description: "Scrape data from a web page via CSS selectors",
            parameters: vec![ParamDef {
                name: "url",
                description: "HTTPS URL to scrape",
                required: true,
                param_type: ParamType::String,
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_7_builtin_tools() {
        let reg = ToolRegistry::new();
        assert_eq!(reg.tools().len(), 7);
    }

    #[test]
    fn find_existing_tool() {
        let reg = ToolRegistry::new();
        assert!(reg.find("bash").is_some());
        assert!(reg.find("read").is_some());
        assert!(reg.find("web_scrape").is_some());
    }

    #[test]
    fn find_nonexistent_returns_none() {
        let reg = ToolRegistry::new();
        assert!(reg.find("nonexistent").is_none());
    }

    #[test]
    fn format_for_prompt_contains_all_tools() {
        let reg = ToolRegistry::new();
        let prompt = reg.format_for_prompt();
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("</tools>"));
        assert!(prompt.contains("## bash"));
        assert!(prompt.contains("## read"));
        assert!(prompt.contains("## edit"));
        assert!(prompt.contains("## write"));
        assert!(prompt.contains("## glob"));
        assert!(prompt.contains("## grep"));
        assert!(prompt.contains("## web_scrape"));
    }

    #[test]
    fn format_for_prompt_shows_param_info() {
        let reg = ToolRegistry::new();
        let prompt = reg.format_for_prompt();
        assert!(prompt.contains("required"));
        assert!(prompt.contains("optional"));
        assert!(prompt.contains("string"));
    }

    #[test]
    fn param_type_display() {
        assert_eq!(ParamType::String.to_string(), "string");
        assert_eq!(ParamType::Integer.to_string(), "integer");
        assert_eq!(ParamType::Boolean.to_string(), "boolean");
    }

    #[test]
    fn default_registry() {
        let reg = ToolRegistry::default();
        assert_eq!(reg.tools().len(), 7);
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
        let reg = ToolRegistry::new();
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
        let reg = ToolRegistry::new();
        let prompt = reg.format_for_prompt_filtered(&policy);
        assert!(prompt.contains("## bash"));
    }

    #[test]
    fn format_filtered_no_rules_includes_all() {
        let policy = crate::permissions::PermissionPolicy::default();
        let reg = ToolRegistry::new();
        let prompt = reg.format_for_prompt_filtered(&policy);
        assert!(prompt.contains("## bash"));
        assert!(prompt.contains("## read"));
    }
}
