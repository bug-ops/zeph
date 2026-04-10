// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured compaction summary with anchored, typed sections.
//!
//! Used during hard compaction when `[memory] structured_summaries = true`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Structured compaction summary with anchored sections.
///
/// Produced by the structured summarization path during hard compaction.
/// Replaces the free-form 9-section prose when `[memory] structured_summaries = true`.
///
/// # Mandatory fields
/// `session_intent` and `next_steps` must be non-empty for the summary to be considered
/// complete. `files_modified` and `decisions_made` are expected but allowed empty (pure
/// discussion sessions may have neither).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnchoredSummary {
    /// What the user is ultimately trying to accomplish in this session.
    pub session_intent: String,
    /// File paths, function names, structs/enums touched or referenced.
    /// Each entry is a path or qualified name.
    pub files_modified: Vec<String>,
    /// Architectural or implementation decisions made, with rationale.
    /// Format: "Decision: `<what>` — Reason: `<why>`".
    pub decisions_made: Vec<String>,
    /// Unresolved questions, ambiguities, or blocked items.
    pub open_questions: Vec<String>,
    /// Concrete next actions the agent should take immediately.
    pub next_steps: Vec<String>,
}

impl AnchoredSummary {
    /// Returns true if the mandatory sections (`session_intent`, `next_steps`) are populated.
    ///
    /// `files_modified` and `decisions_made` are soft expectations: empty is allowed but
    /// triggers a warning log. `open_questions` may always be empty.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.session_intent.trim().is_empty() && !self.next_steps.is_empty()
    }

    /// Render as Markdown for context injection into the LLM.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("[anchored summary]\n");
        out.push_str("## Session Intent\n");
        out.push_str(&self.session_intent);
        out.push('\n');

        if !self.files_modified.is_empty() {
            out.push_str("\n## Files Modified\n");
            for entry in &self.files_modified {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.decisions_made.is_empty() {
            out.push_str("\n## Decisions Made\n");
            for entry in &self.decisions_made {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.open_questions.is_empty() {
            out.push_str("\n## Open Questions\n");
            for entry in &self.open_questions {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.next_steps.is_empty() {
            out.push_str("\n## Next Steps\n");
            for entry in &self.next_steps {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        out
    }

    /// Validate per-field length limits to guard against bloated LLM output.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a descriptive message if any field exceeds its limit.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_INTENT: usize = 2_000;
        const MAX_ENTRY: usize = 500;
        const MAX_VEC_LEN: usize = 50;

        if self.session_intent.len() > MAX_INTENT {
            return Err(format!(
                "session_intent exceeds {MAX_INTENT} chars (got {})",
                self.session_intent.len()
            ));
        }
        for (field, entries) in [
            ("files_modified", &self.files_modified),
            ("decisions_made", &self.decisions_made),
            ("open_questions", &self.open_questions),
            ("next_steps", &self.next_steps),
        ] {
            if entries.len() > MAX_VEC_LEN {
                return Err(format!(
                    "{field} has {} entries (max {MAX_VEC_LEN})",
                    entries.len()
                ));
            }
            for entry in entries {
                if entry.len() > MAX_ENTRY {
                    return Err(format!(
                        "{field} entry exceeds {MAX_ENTRY} chars (got {})",
                        entry.len()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Serialize to JSON for storage in `summaries.content`.
    ///
    /// # Panics
    ///
    /// Panics if serialization fails. Since all fields are `String`/`Vec<String>`,
    /// serialization is infallible in practice.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("AnchoredSummary serialization is infallible")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_summary() -> AnchoredSummary {
        AnchoredSummary {
            session_intent: "Implement structured summarization for context compression".into(),
            files_modified: vec![
                "crates/zeph-memory/src/anchored_summary.rs".into(),
                "crates/zeph-core/src/agent/context/summarization.rs".into(),
            ],
            decisions_made: vec![
                "Decision: use chat_typed_erased — Reason: provider-agnostic structured output"
                    .into(),
            ],
            open_questions: vec!["Should we add per-provider fallback config?".into()],
            next_steps: vec!["Run pre-commit checks".into(), "Create PR".into()],
        }
    }

    #[test]
    fn serde_round_trip() {
        let original = complete_summary();
        let json = original.to_json();
        let parsed: AnchoredSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.session_intent, original.session_intent);
        assert_eq!(parsed.files_modified, original.files_modified);
        assert_eq!(parsed.decisions_made, original.decisions_made);
        assert_eq!(parsed.open_questions, original.open_questions);
        assert_eq!(parsed.next_steps, original.next_steps);
    }

    #[test]
    fn is_complete_all_populated_returns_true() {
        assert!(complete_summary().is_complete());
    }

    #[test]
    fn is_complete_empty_session_intent_returns_false() {
        let mut s = complete_summary();
        s.session_intent = String::new();
        assert!(!s.is_complete());
    }

    #[test]
    fn is_complete_whitespace_only_intent_returns_false() {
        let mut s = complete_summary();
        s.session_intent = "   ".into();
        assert!(!s.is_complete());
    }

    #[test]
    fn is_complete_empty_next_steps_returns_false() {
        let mut s = complete_summary();
        s.next_steps.clear();
        assert!(!s.is_complete());
    }

    #[test]
    fn is_complete_empty_files_modified_still_true() {
        let mut s = complete_summary();
        s.files_modified.clear();
        assert!(s.is_complete());
    }

    #[test]
    fn is_complete_empty_open_questions_still_true() {
        let mut s = complete_summary();
        s.open_questions.clear();
        assert!(s.is_complete());
    }

    #[test]
    fn to_markdown_contains_all_section_headers() {
        let md = complete_summary().to_markdown();
        assert!(
            md.contains("## Session Intent"),
            "missing Session Intent header"
        );
        assert!(
            md.contains("## Files Modified"),
            "missing Files Modified header"
        );
        assert!(
            md.contains("## Decisions Made"),
            "missing Decisions Made header"
        );
        assert!(
            md.contains("## Open Questions"),
            "missing Open Questions header"
        );
        assert!(md.contains("## Next Steps"), "missing Next Steps header");
    }

    #[test]
    fn to_markdown_strips_leading_bullet_from_entries() {
        let s = AnchoredSummary {
            session_intent: "test".into(),
            files_modified: vec!["- some/file.rs".into()],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec!["- do something".into()],
        };
        let md = s.to_markdown();
        // should render as "- some/file.rs", not "- - some/file.rs"
        assert!(
            md.contains("- some/file.rs\n"),
            "double bullet present in files_modified"
        );
        assert!(
            md.contains("- do something\n"),
            "double bullet present in next_steps"
        );
        assert!(!md.contains("- - "), "double bullet must not appear");
    }

    #[test]
    fn to_markdown_skips_empty_optional_sections() {
        let s = AnchoredSummary {
            session_intent: "intent".into(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec!["step".into()],
        };
        let md = s.to_markdown();
        assert!(
            !md.contains("## Files Modified"),
            "empty section should be omitted"
        );
        assert!(
            !md.contains("## Decisions Made"),
            "empty section should be omitted"
        );
        assert!(
            !md.contains("## Open Questions"),
            "empty section should be omitted"
        );
    }

    #[test]
    fn to_json_produces_valid_json() {
        let json = complete_summary().to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("session_intent").is_some());
        assert!(v.get("files_modified").is_some());
        assert!(v.get("decisions_made").is_some());
        assert!(v.get("open_questions").is_some());
        assert!(v.get("next_steps").is_some());
    }

    #[test]
    fn legacy_prose_does_not_parse_as_anchored_summary() {
        let prose = "This is a free-form summary.\n1. User Intent: ...\n2. Files: ...";
        let result = serde_json::from_str::<AnchoredSummary>(prose);
        assert!(
            result.is_err(),
            "legacy prose must not parse as AnchoredSummary"
        );
    }
}
