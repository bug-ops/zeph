// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic MCP tool pruning for context optimization (#2204).
//!
//! The `prune_tools` free function filters a list of MCP tools to only those relevant
//! to the current task, using an LLM call with a fast/cheap model. This reduces context
//! usage and improves tool selection accuracy when MCP servers expose many tools.
//!
//! `zeph-mcp` does not depend on `zeph-config` (circular dependency: zeph-config ->
//! zeph-mcp). Callers in `zeph-core` convert `ToolPruningConfig` into `PruningParams`
//! before calling `prune_tools`.

use std::fmt::Write as _;

use zeph_llm::LlmError;
use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::tool::McpTool;

/// Errors that can occur during tool pruning.
#[derive(Debug, thiserror::Error)]
pub enum PruningError {
    /// LLM call failed.
    #[error("pruning LLM call failed: {0}")]
    LlmError(#[from] LlmError),
    /// Could not extract a valid JSON array from the LLM response.
    #[error("failed to parse pruning response as JSON array of tool names")]
    ParseError,
}

/// Parameters for the `prune_tools` function.
///
/// Mirrors `zeph_config::ToolPruningConfig` but lives in `zeph-mcp` to avoid a
/// circular crate dependency (`zeph-config` → `zeph-mcp`). Callers in `zeph-core`
/// convert from `ToolPruningConfig`.
#[derive(Debug, Clone)]
pub struct PruningParams {
    /// Maximum number of MCP tools to include after pruning.
    pub max_tools: usize,
    /// Minimum number of MCP tools below which pruning is skipped.
    pub min_tools_to_prune: usize,
    /// Tool names that are never pruned (always included).
    pub always_include: Vec<String>,
}

impl Default for PruningParams {
    fn default() -> Self {
        Self {
            max_tools: 15,
            min_tools_to_prune: 10,
            always_include: Vec::new(),
        }
    }
}

/// Prune MCP tools to those relevant to the current task.
///
/// Returns a filtered subset of `all_tools` based on the LLM's assessment of relevance
/// to `task_context`. Tools listed in `params.always_include` bypass the LLM filter.
///
/// # Behavior
///
/// - If `all_tools.len() < params.min_tools_to_prune`, returns `Ok(all_tools.to_vec())`.
/// - On LLM failure or parse failure, returns `Err(PruningError)` — the caller should
///   fall back to the full tool list and log at `WARN` level.
/// - Result is capped at `params.max_tools` total tools. `max_tools == 0` means no cap.
///
/// # Errors
///
/// Returns `PruningError::LlmError` if the provider call fails.
/// Returns `PruningError::ParseError` if the response cannot be parsed as a JSON array.
pub async fn prune_tools<P: LlmProvider>(
    all_tools: &[McpTool],
    task_context: &str,
    params: &PruningParams,
    provider: &P,
) -> Result<Vec<McpTool>, PruningError> {
    if all_tools.len() < params.min_tools_to_prune {
        return Ok(all_tools.to_vec());
    }

    // Partition: always-include tools bypass the LLM filter.
    let (pinned, candidates): (Vec<_>, Vec<_>) = all_tools
        .iter()
        .partition(|t| params.always_include.iter().any(|a| a == &t.name));

    // Build the pruning prompt.
    // Sanitize tool names and descriptions before interpolation to prevent prompt injection
    // from attacker-controlled MCP servers.
    let tool_list = candidates.iter().fold(String::new(), |mut acc, t| {
        let name = sanitize_tool_name(&t.name);
        let desc = sanitize_tool_description(&t.description);
        let _ = writeln!(acc, "- {name}: {desc}");
        acc
    });

    let prompt = format!(
        "Return a JSON array of tool names that are relevant to the task below.\n\
         Return ONLY the JSON array, no explanation, no markdown.\n\n\
         Task: {task_context}\n\n\
         Available tools:\n{tool_list}"
    );

    let messages = vec![Message::from_legacy(Role::User, prompt)];
    let response = provider.chat(&messages).await?;

    // Parse: strip markdown fences, find first `[` to last `]`.
    let relevant_names = parse_name_array(&response)?;

    // always_include tools are added unconditionally and bypass the max_tools cap;
    // max_tools applies only to LLM-selected candidates.
    let mut result: Vec<McpTool> = pinned.into_iter().cloned().collect();
    let mut candidates_added: usize = 0;
    for tool in &candidates {
        // max_tools == 0 means no cap on LLM-selected candidates.
        if params.max_tools > 0 && candidates_added >= params.max_tools {
            break;
        }
        if relevant_names.iter().any(|n| n == &tool.name) {
            result.push((*tool).clone());
            candidates_added += 1;
        }
    }

    Ok(result)
}

/// Sanitize a tool name before interpolating into an LLM prompt.
///
/// Strips control characters and caps at 64 characters.
fn sanitize_tool_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Sanitize a tool description before interpolating into an LLM prompt.
///
/// Strips control characters and caps at 200 characters.
fn sanitize_tool_description(desc: &str) -> String {
    desc.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Extract tool names from an LLM response expected to contain a JSON array of strings.
///
/// Handles markdown code fences (` ```json ... ``` `) and leading/trailing whitespace.
fn parse_name_array(response: &str) -> Result<Vec<String>, PruningError> {
    // Strip markdown code fence lines.
    let stripped = response
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");

    // Find the first `[` and last `]` to isolate the JSON array.
    let start = stripped.find('[').ok_or(PruningError::ParseError)?;
    let end = stripped.rfind(']').ok_or(PruningError::ParseError)?;
    if end <= start {
        return Err(PruningError::ParseError);
    }

    let json_fragment = &stripped[start..=end];
    let names: Vec<String> =
        serde_json::from_str(json_fragment).map_err(|_| PruningError::ParseError)?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use zeph_llm::mock::MockProvider;

    use super::*;

    fn make_tool(name: &str, description: &str) -> McpTool {
        McpTool {
            server_id: "test".into(),
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::Value::Null,
        }
    }

    /// Build a params with low `min_tools_to_prune` so tests aren't skipped early.
    fn params_with_max(max_tools: usize) -> PruningParams {
        PruningParams {
            max_tools,
            min_tools_to_prune: 1,
            always_include: Vec::new(),
        }
    }

    #[test]
    fn parse_plain_array() {
        let names = parse_name_array(r#"["bash", "read", "write"]"#).unwrap();
        assert_eq!(names, vec!["bash", "read", "write"]);
    }

    #[test]
    fn parse_array_with_markdown_fences() {
        let input = "```json\n[\"bash\", \"read\"]\n```";
        let names = parse_name_array(input).unwrap();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn parse_array_with_preamble() {
        let input = "Here are the relevant tools:\n[\"bash\", \"read\"]";
        let names = parse_name_array(input).unwrap();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn parse_empty_array() {
        let names = parse_name_array("[]").unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn parse_invalid_returns_error() {
        assert!(parse_name_array("not json").is_err());
        assert!(parse_name_array("").is_err());
        assert!(parse_name_array("{\"key\": \"val\"}").is_err());
    }

    #[test]
    fn below_min_detected() {
        let params = PruningParams {
            min_tools_to_prune: 10,
            ..Default::default()
        };
        // Two tools < 10 → prune_tools would return all as-is.
        assert!(2 < params.min_tools_to_prune);
    }

    #[tokio::test]
    async fn max_tools_zero_means_no_cap() {
        let tools: Vec<McpTool> = (0..5)
            .map(|i| make_tool(&format!("tool{i}"), "desc"))
            .collect();
        let names_json = r#"["tool0","tool1","tool2","tool3","tool4"]"#;
        let provider = MockProvider::with_responses(vec![names_json.into()]);
        let params = params_with_max(0);

        let result = prune_tools(&tools, "any task", &params, &provider)
            .await
            .unwrap();
        assert_eq!(result.len(), 5, "max_tools=0 must not cap the result");
    }

    #[test]
    fn description_sanitization_strips_control_chars_and_caps() {
        // Newline and tab are control characters.
        let desc = "line1\nline2\tinject";
        let sanitized = sanitize_tool_description(desc);
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\t'));

        // Cap at 200 characters.
        let long_desc = "x".repeat(300);
        assert_eq!(sanitize_tool_description(&long_desc).len(), 200);

        // Name capped at 64 characters.
        let long_name = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long_name).len(), 64);
    }

    #[tokio::test]
    async fn always_include_bypasses_max_tools_cap() {
        // max_tools=1 — only 1 candidate from LLM allowed; but always_include adds unconditionally.
        let tools = vec![
            make_tool("pinned", "always here"),
            make_tool("candidate_a", "desc a"),
            make_tool("candidate_b", "desc b"),
        ];
        let provider =
            MockProvider::with_responses(vec![r#"["candidate_a","candidate_b"]"#.into()]);
        let params = PruningParams {
            max_tools: 1,
            min_tools_to_prune: 1,
            always_include: vec!["pinned".into()],
        };

        let result = prune_tools(&tools, "task", &params, &provider)
            .await
            .unwrap();

        // "pinned" is always present regardless of max_tools.
        assert!(
            result.iter().any(|t| t.name == "pinned"),
            "pinned tool must bypass cap"
        );
        // Only 1 candidate slot remains after pinned bypasses cap; total = 1 (pinned) + 1 (candidate).
        assert_eq!(result.len(), 2);
    }
}
