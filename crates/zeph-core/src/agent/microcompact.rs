// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Time-based microcompact (#2699).
//!
//! Clears stale low-value tool output from context when the session has been
//! idle longer than `gap_threshold_minutes`. This is a zero-LLM-cost in-memory
//! operation that reduces context pressure before compaction runs.

use zeph_llm::provider::MessagePart;

use crate::channel::Channel;

/// Tool names whose output is considered low-value after a session gap.
///
/// Case-insensitive comparison is used at the call site.
const LOW_VALUE_TOOLS: &[&str] = &[
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
const CLEARED_SENTINEL_PREFIX: &str = "[cleared";

impl<C: Channel> super::Agent<C> {
    /// Clear stale low-value tool output when the session gap exceeds the configured threshold.
    ///
    /// No-op when:
    /// - microcompact is disabled
    /// - `last_assistant_at` is `None` (first turn)
    /// - idle gap is below the threshold
    pub(super) fn maybe_time_based_microcompact(&mut self) {
        let cfg = &self.memory_state.microcompact_config;
        if !cfg.enabled {
            return;
        }

        let Some(last_at) = self.session.last_assistant_at else {
            return;
        };

        let elapsed_mins = last_at.elapsed().as_secs_f64() / 60.0;
        if elapsed_mins < f64::from(cfg.gap_threshold_minutes) {
            return;
        }

        tracing::debug!(
            elapsed_mins = %format!("{elapsed_mins:.1}"),
            gap_threshold = cfg.gap_threshold_minutes,
            "time-based microcompact: gap exceeded, sweeping stale tool outputs"
        );

        let keep_recent = cfg.keep_recent;
        let messages = &mut self.msg.messages;

        // Collect indices of compactable tool output parts across all messages.
        // We track (msg_idx, part_idx) for ToolOutput, and msg_idx for ToolResult messages.
        let mut compactable: Vec<(usize, CompactTarget)> = Vec::new();

        for (msg_idx, msg) in messages.iter().enumerate() {
            // Only assistant messages carry ToolOutput; user messages carry ToolResult.
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
                            || !is_low_value_tool(tool_name)
                        {
                            continue;
                        }
                        compactable.push((msg_idx, CompactTarget::Output(part_idx)));
                    }
                    MessagePart::ToolResult { content, .. } => {
                        // ToolResult has no compacted_at field — use sentinel detection.
                        if content.starts_with(CLEARED_SENTINEL_PREFIX) {
                            continue;
                        }
                        // For ToolResult we need to know the associated tool name.
                        // Walk back in the same message to find the preceding ToolUse part.
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
            return;
        }

        // Preserve the keep_recent most recent compactable entries.
        let clear_count = total.saturating_sub(keep_recent);
        if clear_count == 0 {
            tracing::debug!(
                total,
                keep_recent,
                "microcompact: all within keep_recent window, skipping"
            );
            return;
        }

        let sentinel = format!("[cleared — stale tool output after {elapsed_mins:.0}min idle]");
        let now_ts = chrono::Utc::now().timestamp();

        for (msg_idx, target) in &compactable[..clear_count] {
            let msg = &mut messages[*msg_idx];
            match target {
                CompactTarget::Output(part_idx) => {
                    if let MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } = &mut msg.parts[*part_idx]
                    {
                        body.clone_from(&sentinel);
                        *compacted_at = Some(now_ts);
                    }
                }
                CompactTarget::Result(part_idx) => {
                    if let MessagePart::ToolResult { content, .. } = &mut msg.parts[*part_idx] {
                        content.clone_from(&sentinel);
                    }
                }
            }
        }

        tracing::debug!(
            cleared = clear_count,
            preserved = keep_recent,
            "microcompact: cleared stale tool outputs"
        );
    }
}

#[derive(Debug)]
enum CompactTarget {
    Output(usize),
    Result(usize),
}

/// Returns the tool name from the closest preceding `ToolUse` part, if any.
fn find_preceding_tool_use_name(parts: &[MessagePart], result_idx: usize) -> Option<&str> {
    // Walk backward from result_idx - 1 looking for ToolUse.
    for part in parts[..result_idx].iter().rev() {
        if let MessagePart::ToolUse { name, .. } = part {
            return Some(name.as_str());
        }
    }
    None
}

/// Returns `true` if `tool_name` (case-insensitive) is in the low-value set.
fn is_low_value_tool(tool_name: &str) -> bool {
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
        use zeph_llm::provider::MessagePart;
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
        use zeph_llm::provider::MessagePart;
        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "1".into(),
            content: "output".into(),
            is_error: false,
        }];
        let name = find_preceding_tool_use_name(&parts, 0);
        assert!(name.is_none());
    }
}
