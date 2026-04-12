// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure helpers for time-based microcompact (#2699).
//!
//! The `Agent`-level integration (reading `self.*`, mutating message history)
//! lives in `zeph-core`. This module contains only the stateless helpers.

use zeph_llm::provider::{Message, MessagePart};

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

/// Index into a message's parts list identifying which part to compact.
#[derive(Debug)]
pub enum CompactTarget {
    /// A `ToolOutput` part at the given index.
    Output(usize),
    /// A `ToolResult` part at the given index.
    Result(usize),
}

/// Sweep stale low-value tool outputs from the message list.
///
/// Clears all but the most recent `keep_recent` compactable outputs, replacing their
/// content with `sentinel`. The `now_ts` parameter is the current Unix timestamp
/// (seconds) used to mark `compacted_at` on `ToolOutput` parts.
///
/// Returns the number of cleared entries.
pub fn sweep_stale_tool_outputs(
    messages: &mut [Message],
    keep_recent: usize,
    sentinel: &str,
    now_ts: i64,
) -> usize {
    let mut compactable: Vec<(usize, CompactTarget)> = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        for (part_idx, part) in msg.parts.iter().enumerate() {
            match part {
                MessagePart::ToolOutput {
                    tool_name,
                    body,
                    compacted_at,
                    ..
                } => {
                    if compacted_at.is_some()
                        || body.starts_with(CLEARED_SENTINEL_PREFIX)
                        || !is_low_value_tool(tool_name.as_str())
                    {
                        continue;
                    }
                    compactable.push((msg_idx, CompactTarget::Output(part_idx)));
                }
                MessagePart::ToolResult { content, .. } => {
                    if content.starts_with(CLEARED_SENTINEL_PREFIX) {
                        continue;
                    }
                    let tool_name = find_preceding_tool_use_name(&msg.parts, part_idx);
                    if let Some(name) = tool_name
                        && is_low_value_tool(name)
                    {
                        compactable.push((msg_idx, CompactTarget::Result(part_idx)));
                    }
                }
                _ => {}
            }
        }
    }

    let total = compactable.len();
    if total == 0 {
        return 0;
    }

    let clear_count = total.saturating_sub(keep_recent);
    if clear_count == 0 {
        return 0;
    }

    for (msg_idx, target) in &compactable[..clear_count] {
        let msg = &mut messages[*msg_idx];
        match target {
            CompactTarget::Output(part_idx) => {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = &mut msg.parts[*part_idx]
                {
                    body.clone_from(&sentinel.to_string());
                    *compacted_at = Some(now_ts);
                }
            }
            CompactTarget::Result(part_idx) => {
                if let MessagePart::ToolResult { content, .. } = &mut msg.parts[*part_idx] {
                    content.clone_from(&sentinel.to_string());
                }
            }
        }
    }

    clear_count
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

    fn tool_output_msg(tool_name: &str, body: &str) -> Message {
        use zeph_llm::provider::{MessageMetadata, Role};
        Message {
            role: Role::User,
            content: body.to_string(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: tool_name.into(),
                body: body.into(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        }
    }

    fn tool_result_msg(tool_name: &str, content: &str) -> Message {
        use zeph_llm::provider::{MessageMetadata, Role};
        Message {
            role: Role::User,
            content: content.to_string(),
            parts: vec![
                MessagePart::ToolUse {
                    id: "id".into(),
                    name: tool_name.into(),
                    input: serde_json::Value::Null,
                },
                MessagePart::ToolResult {
                    tool_use_id: "id".into(),
                    content: content.into(),
                    is_error: false,
                },
            ],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn sweep_clears_all_when_keep_recent_zero() {
        let mut messages = vec![
            tool_output_msg("bash", "output1"),
            tool_output_msg("grep", "output2"),
            tool_output_msg("shell", "output3"),
        ];
        let cleared = sweep_stale_tool_outputs(&mut messages, 0, "[cleared]", 1000);
        assert_eq!(cleared, 3);
        for msg in &messages {
            if let MessagePart::ToolOutput {
                body, compacted_at, ..
            } = &msg.parts[0]
            {
                assert_eq!(body, "[cleared]");
                assert_eq!(*compacted_at, Some(1000));
            }
        }
    }

    #[test]
    fn sweep_preserves_keep_recent_most_recent() {
        let mut messages = vec![
            tool_output_msg("bash", "output1"),
            tool_output_msg("grep", "output2"),
            tool_output_msg("shell", "output3"),
        ];
        let cleared = sweep_stale_tool_outputs(&mut messages, 2, "[cleared]", 1000);
        // 3 total - 2 keep_recent = 1 cleared
        assert_eq!(cleared, 1);
        // first message cleared
        if let MessagePart::ToolOutput { body, .. } = &messages[0].parts[0] {
            assert_eq!(body, "[cleared]");
        }
        // last two preserved
        if let MessagePart::ToolOutput { body, .. } = &messages[1].parts[0] {
            assert_eq!(body, "output2");
        }
        if let MessagePart::ToolOutput { body, .. } = &messages[2].parts[0] {
            assert_eq!(body, "output3");
        }
    }

    #[test]
    fn sweep_is_idempotent_on_already_cleared() {
        let mut messages = vec![
            tool_output_msg("bash", "[cleared — stale]"),
            tool_output_msg("grep", "output2"),
        ];
        // First message already cleared — only 1 compactable, keep_recent=0 → clear 1
        let cleared = sweep_stale_tool_outputs(&mut messages, 0, "[cleared]", 1000);
        assert_eq!(cleared, 1);
        // Already-cleared message body should be unchanged (it was skipped)
        if let MessagePart::ToolOutput { body, .. } = &messages[0].parts[0] {
            assert_eq!(body, "[cleared — stale]");
        }
        // Second message should now be cleared
        if let MessagePart::ToolOutput { body, .. } = &messages[1].parts[0] {
            assert_eq!(body, "[cleared]");
        }
    }

    #[test]
    fn sweep_skips_high_value_tools() {
        let mut messages = vec![
            tool_output_msg("file_edit", "important"),
            tool_output_msg("bash", "output"),
        ];
        let cleared = sweep_stale_tool_outputs(&mut messages, 0, "[cleared]", 1000);
        // file_edit is high-value, only bash is compactable
        assert_eq!(cleared, 1);
        if let MessagePart::ToolOutput { body, .. } = &messages[0].parts[0] {
            assert_eq!(
                body, "important",
                "high-value tool output must be preserved"
            );
        }
        if let MessagePart::ToolOutput { body, .. } = &messages[1].parts[0] {
            assert_eq!(body, "[cleared]");
        }
    }

    #[test]
    fn sweep_clears_tool_result_parts() {
        let mut messages = vec![
            tool_result_msg("bash", "result1"),
            tool_result_msg("grep", "result2"),
        ];
        let cleared = sweep_stale_tool_outputs(&mut messages, 0, "[cleared]", 1000);
        assert_eq!(cleared, 2);
        for msg in &messages {
            if let MessagePart::ToolResult { content, .. } = &msg.parts[1] {
                assert_eq!(content, "[cleared]");
            }
        }
    }
}
