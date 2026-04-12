// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure helpers for time-based microcompact (#2699).
//!
//! The `Agent`-level integration (reading `self.*`, mutating message history)
//! lives in `zeph-core`. This module contains only the stateless helpers.

use zeph_llm::provider::MessagePart;

/// Tool names whose output is considered low-value after a session gap.
///
/// Case-insensitive comparison is used at the call site.
pub const LOW_VALUE_TOOLS: &[&str] = &[
    "bash",
    "shell",
    "grep",
    "rg",
    "ripgrep",
    "glob",
    "find",
    "web_fetch",
    "fetch",
    "web_search",
    "search",
    "read",
    "cat",
    "list_directory",
];

/// Sentinel content placed in cleared tool outputs.
///
/// Prefixed with `[cleared` so reload detection can skip already-cleared parts.
pub const CLEARED_SENTINEL_PREFIX: &str = "[cleared";

/// Returns the tool name from the closest preceding `ToolUse` part, if any.
///
/// Walks backward from `result_idx - 1` looking for a `ToolUse` variant.
#[must_use]
pub fn find_preceding_tool_use_name(parts: &[MessagePart], result_idx: usize) -> Option<&str> {
    for part in parts[..result_idx].iter().rev() {
        if let MessagePart::ToolUse { name, .. } = part {
            return Some(name.as_str());
        }
    }
    None
}

/// Returns `true` if `tool_name` (case-insensitive) is in the low-value set.
#[must_use]
pub fn is_low_value_tool(tool_name: &str) -> bool {
    let lower = tool_name.to_lowercase();
    LOW_VALUE_TOOLS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_value_tool_detection_case_insensitive() {
        assert!(is_low_value_tool("Bash"));
        assert!(is_low_value_tool("GREP"));
        assert!(is_low_value_tool("list_directory"));
        assert!(!is_low_value_tool("file_edit"));
        assert!(!is_low_value_tool("memory_save"));
        assert!(!is_low_value_tool("mcp_tool"));
    }

    #[test]
    fn find_preceding_tool_use_name_returns_closest() {
        let parts = vec![
            MessagePart::ToolUse {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::Value::Null,
            },
            MessagePart::ToolResult {
                tool_use_id: "1".into(),
                content: "output".into(),
                is_error: false,
            },
        ];
        let name = find_preceding_tool_use_name(&parts, 1);
        assert_eq!(name, Some("bash"));
    }

    #[test]
    fn find_preceding_tool_use_name_no_match() {
        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "1".into(),
            content: "output".into(),
            is_error: false,
        }];
        let name = find_preceding_tool_use_name(&parts, 0);
        assert!(name.is_none());
    }
}
