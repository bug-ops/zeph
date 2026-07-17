// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared transcript formatting for `/history` (spec-068 §13.6).
//!
//! [`TranscriptFormatter`] is the single source of truth for rendering a bounded slice of
//! conversation history into role-prefixed, tool-collapsed text. Both the flat-text channels
//! (CLI, Telegram, Discord, Slack) and the TUI backfill path reuse it — neither implements its
//! own formatting.

/// Role of a [`TranscriptEntry`], decoupled from `zeph_llm::provider::Role` so this crate does
/// not depend on `zeph-llm` (see the crate-level DRY/dependency note in `lib.rs`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    /// A user turn.
    User,
    /// An assistant turn (text reply).
    Assistant,
    /// A tool call/result, collapsed to a single line.
    Tool,
}

/// One formattable entry in a bounded transcript slice.
///
/// Produced by `MessageAccess::transcript_page` — already bounded before this type exists, per
/// INV-SP-6 (never materialize-then-trim).
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    /// Speaker role.
    pub role: TranscriptRole,
    /// Display text for this entry (already extracted from structured message parts).
    pub content: String,
    /// Tool name, set only when `role == Tool`.
    pub tool_name: Option<String>,
}

/// Formats bounded [`TranscriptEntry`] slices into role-prefixed, tool-collapsed text.
pub struct TranscriptFormatter;

impl TranscriptFormatter {
    /// Render entries as a single newline-joined, role-prefixed string.
    ///
    /// Used by every channel that has no structured display buffer of its own (CLI, Telegram,
    /// Discord, Slack). The TUI backfill path instead pushes each entry individually into its
    /// own chat message buffer, but still sources entries from the same
    /// `MessageAccess::transcript_page` bounded slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_commands::transcript::{TranscriptEntry, TranscriptFormatter, TranscriptRole};
    ///
    /// let entries = vec![TranscriptEntry {
    ///     role: TranscriptRole::User,
    ///     content: "hello".to_owned(),
    ///     tool_name: None,
    /// }];
    /// let text = TranscriptFormatter::render_flat(&entries);
    /// assert_eq!(text, "user: hello");
    /// ```
    #[must_use]
    pub fn render_flat(entries: &[TranscriptEntry]) -> String {
        entries
            .iter()
            .map(Self::render_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render a single entry as one role-prefixed, tool-collapsed line.
    #[must_use]
    pub fn render_line(entry: &TranscriptEntry) -> String {
        match entry.role {
            TranscriptRole::User => format!("user: {}", entry.content),
            TranscriptRole::Assistant => format!("assistant: {}", entry.content),
            TranscriptRole::Tool => {
                let name = entry.tool_name.as_deref().unwrap_or("tool");
                format!("[tool: {name}] {}", entry.content)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_flat_joins_lines_in_order() {
        let entries = vec![
            TranscriptEntry {
                role: TranscriptRole::User,
                content: "hi".to_owned(),
                tool_name: None,
            },
            TranscriptEntry {
                role: TranscriptRole::Assistant,
                content: "hello".to_owned(),
                tool_name: None,
            },
        ];
        let text = TranscriptFormatter::render_flat(&entries);
        assert_eq!(text, "user: hi\nassistant: hello");
    }

    #[test]
    fn render_line_collapses_tool_entry() {
        let entry = TranscriptEntry {
            role: TranscriptRole::Tool,
            content: "$ ls\nfile.txt".to_owned(),
            tool_name: Some("bash".to_owned()),
        };
        let line = TranscriptFormatter::render_line(&entry);
        assert!(line.starts_with("[tool: bash]"));
    }

    #[test]
    fn render_line_tool_without_name_falls_back() {
        let entry = TranscriptEntry {
            role: TranscriptRole::Tool,
            content: "output".to_owned(),
            tool_name: None,
        };
        let line = TranscriptFormatter::render_line(&entry);
        assert!(line.starts_with("[tool: tool]"));
    }

    #[test]
    fn render_flat_empty_slice_is_empty_string() {
        assert_eq!(TranscriptFormatter::render_flat(&[]), "");
    }
}
