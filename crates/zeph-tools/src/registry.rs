use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationHint {
    /// Tool invoked via ```{tag}\n...\n``` fenced block in LLM response
    FencedBlock(&'static str),
    /// Tool invoked via structured `ToolCall` JSON
    ToolCall,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub id: &'static str,
    pub description: &'static str,
    pub schema: schemars::Schema,
    pub invocation: InvocationHint,
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
        self.tools.iter().find(|t| t.id == id)
    }

    /// Format tools for prompt, excluding tools fully denied by policy.
    #[must_use]
    pub fn format_for_prompt_filtered(
        &self,
        policy: &crate::permissions::PermissionPolicy,
    ) -> String {
        let mut out = String::from("<tools>\n");
        for tool in &self.tools {
            if policy.is_fully_denied(tool.id) {
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
    use crate::file::ReadParams;
    use crate::shell::BashParams;

    fn sample_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "bash",
                description: "Execute a shell command",
                schema: schemars::schema_for!(BashParams),
                invocation: InvocationHint::FencedBlock("bash"),
            },
            ToolDef {
                id: "read",
                description: "Read file contents",
                schema: schemars::schema_for!(ReadParams),
                invocation: InvocationHint::ToolCall,
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
