// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured compaction summary with anchored, typed sections.
//!
//! Used during hard compaction when `[memory] structured_summaries = true`.
//! The type itself lives in `zeph-common::memory`; this module re-exports it
//! so that existing `zeph_memory::AnchoredSummary` paths continue to resolve.

pub use zeph_common::memory::AnchoredSummary;

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
