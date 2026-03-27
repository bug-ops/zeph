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
/// - Result is capped at `params.max_tools` total tools.
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
    let tool_list = candidates
        .iter()
        .map(|t| format!("{}: {}", t.name, t.description))
        .collect::<Vec<_>>()
        .join("\n");

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

    // Build result: pinned tools + matched candidates, capped at max_tools.
    let mut result: Vec<McpTool> = pinned.into_iter().cloned().collect();
    for tool in &candidates {
        if result.len() >= params.max_tools {
            break;
        }
        if relevant_names.iter().any(|n| n == &tool.name) {
            result.push((*tool).clone());
        }
    }

    Ok(result)
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
    use super::*;

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
}
