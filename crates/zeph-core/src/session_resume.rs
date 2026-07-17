// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resume-visibility presentation primitive (spec-068 §13).
//!
//! [`SessionResumeInfo`] is computed once at hydration time from the already-reconstructed
//! message stream (`ReplayEngine::fold`'s output, or the `SQLite` fallback when
//! `[session] enabled = false`) and rendered per-channel: a neutral banner on CLI/TUI
//! (display-owning channels), nothing on chat/ACP channels (spec-068 §13.2, §13.8).
//!
//! This module adds zero I/O: it only inspects a message slice already materialized by the
//! caller's hydration path.

use zeph_llm::provider::{Message, Role};

/// Presentation-only summary of a conversation-session's prior state, computed at hydration.
///
/// Carries no full message vector — expansion is pulled lazily by `/history`, never eagerly
/// (spec-068 §13.3).
#[derive(Debug, Clone)]
pub struct SessionResumeInfo {
    /// Whether this hydration reconstructed a non-empty prior conversation (§13.4).
    pub is_resume: bool,
    /// Count of non-system messages in the reconstructed stream.
    pub prior_message_count: usize,
    /// Approximate number of turns (count of user messages in the reconstructed stream).
    pub prior_turn_count: usize,
    /// Last-active timestamp, formatted for display (e.g. `"2h ago"`), when known.
    pub last_active: Option<String>,
}

impl SessionResumeInfo {
    /// Compute resume info from an already-reconstructed message stream.
    ///
    /// `is_resume` is evaluated against the raw message stream — at least one non-system
    /// message (`User`, `Assistant`, or a tool-bearing message) makes this `true`, even when
    /// the only assistant message is tool-use-only with no visible text (spec-068 §13.4,
    /// AC-17). This must never be computed against a display-filtered/visible-text-only turn
    /// count — that would false-negative a session interrupted mid-tool-loop as fresh.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::session_resume::SessionResumeInfo;
    /// use zeph_llm::provider::{Message, Role};
    ///
    /// let messages = vec![Message::from_legacy(Role::System, "system prompt")];
    /// let info = SessionResumeInfo::from_messages(&messages, None);
    /// assert!(!info.is_resume, "system-prompt-only history is fresh, not a resume");
    /// ```
    #[must_use]
    pub fn from_messages(messages: &[Message], last_active_raw: Option<&str>) -> Self {
        let non_system: Vec<&Message> =
            messages.iter().filter(|m| m.role != Role::System).collect();
        let prior_turn_count = non_system.iter().filter(|m| m.role == Role::User).count();
        Self {
            is_resume: !non_system.is_empty(),
            prior_message_count: non_system.len(),
            prior_turn_count,
            last_active: last_active_raw.and_then(format_last_active),
        }
    }

    /// Render the neutral CLI/TUI banner line (spec-068 §13.5).
    ///
    /// Carries no interrupted/clean-exit qualifier in v1 (§13.10). Returns `None` when
    /// `is_resume` is `false` — callers must not render anything for a fresh conversation
    /// (AC-16).
    #[must_use]
    pub fn banner_text(&self) -> Option<String> {
        if !self.is_resume {
            return None;
        }
        let turns = if self.prior_turn_count == 1 {
            "1 turn".to_owned()
        } else {
            format!("{} turns", self.prior_turn_count)
        };
        let messages = if self.prior_message_count == 1 {
            "1 message".to_owned()
        } else {
            format!("{} messages", self.prior_message_count)
        };
        Some(match &self.last_active {
            Some(last_active) => format!(
                "\u{21bb} Resuming session (last active {last_active}) — {messages}, {turns}. Type /history to view."
            ),
            None => {
                format!("\u{21bb} Resuming session — {messages}, {turns}. Type /history to view.")
            }
        })
    }
}

/// Format a `SQLite`/`PostgreSQL` `updated_at` string (`"YYYY-MM-DD HH:MM:SS"`, UTC) as a
/// coarse relative-time string (e.g. `"2h ago"`). Returns `None` if the string cannot be
/// parsed rather than surfacing a raw, potentially confusing timestamp.
fn format_last_active(raw: &str) -> Option<String> {
    let parsed = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()?;
    let then = parsed.and_utc();
    let now = chrono::Utc::now();
    let secs = now.signed_duration_since(then).num_seconds().max(0);
    Some(if secs < 60 {
        "just now".to_owned()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::MessagePart;

    fn msg(role: Role, content: &str) -> Message {
        Message::from_legacy(role, content)
    }

    #[test]
    fn fresh_conversation_system_only_is_not_resume() {
        let messages = vec![msg(Role::System, "system prompt")];
        let info = SessionResumeInfo::from_messages(&messages, None);
        assert!(!info.is_resume);
        assert_eq!(info.prior_message_count, 0);
        assert!(info.banner_text().is_none());
    }

    #[test]
    fn normal_conversation_is_resume() {
        let messages = vec![
            msg(Role::System, "system prompt"),
            msg(Role::User, "hello"),
            msg(Role::Assistant, "hi there"),
        ];
        let info = SessionResumeInfo::from_messages(&messages, None);
        assert!(info.is_resume);
        assert_eq!(info.prior_message_count, 2);
        assert_eq!(info.prior_turn_count, 1);
        assert!(info.banner_text().unwrap().contains("Resuming session"));
    }

    /// Regression test for the M-REV2-2 predicate fix (spec-068 §13.4, AC-17): a session
    /// interrupted mid-tool-loop reconstructs to `[system, assistant(tool_use-only),
    /// user(tool_result)]` — no assistant text — and must still evaluate `is_resume = true`.
    #[test]
    fn mid_tool_loop_interruption_is_still_resume() {
        let mut tool_use_only = msg(Role::Assistant, "");
        tool_use_only.parts.push(MessagePart::ToolUse {
            id: "toolu_1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        let mut tool_result = msg(Role::User, "");
        tool_result.parts.push(MessagePart::ToolResult {
            tool_use_id: "toolu_1".to_owned(),
            content: "file.txt".to_owned(),
            is_error: false,
        });
        let messages = vec![
            msg(Role::System, "system prompt"),
            tool_use_only,
            tool_result,
        ];
        let info = SessionResumeInfo::from_messages(&messages, None);
        assert!(
            info.is_resume,
            "mid-tool-loop interruption must evaluate as resume, not fresh"
        );
        assert_eq!(info.prior_message_count, 2);
    }

    #[test]
    fn last_active_formats_recent_seconds_as_just_now() {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let formatted = format_last_active(&now).expect("must parse");
        assert_eq!(formatted, "just now");
    }

    #[test]
    fn last_active_unparsable_returns_none() {
        assert!(format_last_active("not-a-date").is_none());
    }

    #[test]
    fn banner_singular_turn_and_message_grammar() {
        let messages = vec![msg(Role::System, "sp"), msg(Role::User, "hi")];
        let info = SessionResumeInfo::from_messages(&messages, None);
        let text = info.banner_text().unwrap();
        assert!(text.contains("1 message"));
        assert!(text.contains("1 turn"));
        assert!(!text.contains("1 turns"));
    }
}
