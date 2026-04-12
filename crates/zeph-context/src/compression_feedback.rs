// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure detection and classification helpers for ACON compression guidelines (#1647).
//!
//! This module contains the stateless functions that detect context loss after
//! compaction and classify what was likely lost. The `Agent`-level integration
//! (logging to `SQLite`, reading `self.*` fields) lives in `zeph-core`.

use std::sync::LazyLock;

use regex::Regex;

/// Explicit uncertainty phrases — signals the agent is unaware of something.
///
/// Pattern set 1: must match for detection to fire.
static UNCERTAINTY_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(
            r"(?i)\b(i\s+(don'?t|no\s+longer)\s+have\s+(access\s+to|information\s+(about|on|regarding)))\b",
        )
        .unwrap(),
        Regex::new(r"(?i)\b(i\s+(wasn'?t|haven'?t\s+been)\s+(provided|given)\s+with)\b").unwrap(),
        Regex::new(
            r"(?i)\b(i\s+don'?t\s+(recall|remember)\s+(any|the|what|which)\s+(previous|earlier|prior|specific))\b",
        )
        .unwrap(),
        Regex::new(
            r"(?i)\b(i'?m\s+not\s+sure\s+what\s+(we|was|had)\s+(discussed|covered|decided|established))\b",
        )
        .unwrap(),
        Regex::new(r"(?i)\b(could\s+you\s+(remind|tell)\s+me\s+(what|again|about))\b").unwrap(),
    ]
});

/// Prior-context reference phrases — signals the response references something that
/// should have been in the compressed context.
///
/// Pattern set 2: must ALSO match for detection to fire.
static PRIOR_CONTEXT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(
            r"(?i)\b(earlier\s+in\s+(our|this)\s+(conversation|session|chat|discussion))\b",
        )
        .unwrap(),
        Regex::new(
            r"(?i)\b((previously|before)\s+(you\s+(mentioned|said|told|described|shared)|we\s+(discussed|covered|established|decided)))\b",
        )
        .unwrap(),
        Regex::new(
            r"(?i)\b(in\s+(our|the)\s+(earlier|previous|prior|past)\s+(exchange|discussion|conversation|messages|context))\b",
        )
        .unwrap(),
        Regex::new(
            r"(?i)\b(based\s+on\s+what\s+(you|we)\s+(told|said|discussed|shared)(\s+\w+)?\s+(earlier|before|previously))\b",
        )
        .unwrap(),
    ]
});

/// Classify which content category was likely lost in a compaction failure.
///
/// Returns one of: `tool_output`, `assistant_reasoning`, `user_context`, `unknown`.
///
/// Classification is performed on the compaction summary text, not on the
/// post-summary LLM response.
#[must_use]
pub fn classify_failure_category(compressed_context: &str) -> &'static str {
    let has_tool_markers = compressed_context.contains("[tool output")
        || compressed_context.contains("ToolOutput")
        || compressed_context.contains("ToolResult")
        || compressed_context.contains("[archived:")
        || compressed_context.contains("read_overflow");

    let has_user_markers = compressed_context.contains("[user]:")
        || compressed_context
            .matches("[user]:")
            .count()
            .saturating_mul(2)
            > compressed_context.matches("[assistant]:").count();

    let has_assistant_markers =
        compressed_context.contains("[assistant]:") && !has_tool_markers && !has_user_markers;

    if has_tool_markers {
        "tool_output"
    } else if has_assistant_markers {
        "assistant_reasoning"
    } else if has_user_markers {
        "user_context"
    } else {
        "unknown"
    }
}

/// Detect whether `response` contains signals of context loss after compaction.
///
/// Returns `Some(reason)` only when ALL of:
/// 1. `had_compaction` is `true`
/// 2. At least one uncertainty phrase matches
/// 3. At least one prior-context reference phrase also matches
///
/// This two-signal requirement minimizes false positives. Neither pattern set
/// alone is reliable; together they are highly specific.
///
/// # Returns
///
/// - `None` when `had_compaction` is `false`.
/// - `None` when the response does not match both signal categories.
/// - `Some(reason)` with a description string when context loss is detected.
#[must_use]
pub fn detect_compression_failure(response: &str, had_compaction: bool) -> Option<String> {
    if !had_compaction {
        return None;
    }

    let uncertainty_match = UNCERTAINTY_PATTERNS
        .iter()
        .find_map(|p| p.find(response).map(|m| m.as_str().to_string()));

    let prior_ctx_match = PRIOR_CONTEXT_PATTERNS
        .iter()
        .find_map(|p| p.find(response).map(|m| m.as_str().to_string()));

    match (uncertainty_match, prior_ctx_match) {
        (Some(u), Some(p)) => Some(format!(
            "context loss signals detected: uncertainty='{u}', prior-ref='{p}'"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── True positives: both signals present ──────────────────────────────────

    #[test]
    fn detects_dont_have_access_with_prior_ref() {
        let response = "I don't have access to the information about the file path. Earlier in our conversation you mentioned a specific path.";
        assert!(
            detect_compression_failure(response, true).is_some(),
            "should detect context loss"
        );
    }

    #[test]
    fn detects_wasnt_provided_with_previously() {
        let response = "I wasn't provided with that information. Previously you mentioned the database schema.";
        assert!(detect_compression_failure(response, true).is_some());
    }

    #[test]
    fn detects_dont_recall_prior_specific_with_earlier() {
        let response = "I don't recall the specific error from before. In our earlier discussion you shared the stack trace.";
        assert!(detect_compression_failure(response, true).is_some());
    }

    #[test]
    fn detects_not_sure_what_we_discussed_with_prior() {
        let response = "I'm not sure what we discussed about the API design. Based on what you told me earlier, it was REST-based.";
        assert!(detect_compression_failure(response, true).is_some());
    }

    // ── False negatives when had_compaction=false ─────────────────────────────

    #[test]
    fn no_detection_when_no_compaction() {
        let response =
            "I don't have access to that. Earlier in our conversation you mentioned the path.";
        assert!(
            detect_compression_failure(response, false).is_none(),
            "must not fire without compaction"
        );
    }

    // ── False positives: only one signal → should NOT fire ────────────────────

    #[test]
    fn no_detection_with_only_uncertainty() {
        let response = "I don't recall the specific previous details.";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "uncertainty alone must not fire without a prior-context anchor phrase"
        );
    }

    #[test]
    fn no_detection_normal_conversation_reference() {
        let response = "As mentioned earlier, the function takes two arguments.";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "normal conversational reference must not trigger"
        );
    }

    #[test]
    fn no_detection_llm_asking_clarifying_question() {
        let response = "Could you tell me more about what you'd like the function to do?";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "clarifying question without prior-context ref must not fire"
        );
    }

    #[test]
    fn no_detection_legitimate_i_dont_see_previous() {
        let response =
            "I don't see any previous error logs in your message. Could you paste them here?";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "legitimate 'I don't see' without prior-context ref must not fire"
        );
    }

    #[test]
    fn no_detection_empty_response() {
        assert!(detect_compression_failure("", true).is_none());
        assert!(detect_compression_failure("", false).is_none());
    }

    #[test]
    fn returns_reason_string_with_matches() {
        let response = "I don't have access to information about that. Previously you mentioned the config file.";
        let reason = detect_compression_failure(response, true).expect("should detect");
        assert!(
            reason.contains("uncertainty="),
            "reason must include uncertainty match"
        );
        assert!(
            reason.contains("prior-ref="),
            "reason must include prior-ref match"
        );
    }

    // ── classify_failure_category() unit tests ────────────────────────────────

    #[test]
    fn classify_tool_output_by_tool_output_marker() {
        let ctx = "[tool output]: file listing returned 42 items";
        assert_eq!(classify_failure_category(ctx), "tool_output");
    }

    #[test]
    fn classify_tool_output_by_archived_marker() {
        let ctx = "[archived:550e8400-e29b-41d4-a716-446655440000 — tool: shell — 1024 bytes]";
        assert_eq!(classify_failure_category(ctx), "tool_output");
    }

    #[test]
    fn classify_tool_output_by_tooloutput_struct_name() {
        let ctx = "ToolOutput { body: \"...\", tool_name: \"shell\" }";
        assert_eq!(classify_failure_category(ctx), "tool_output");
    }

    #[test]
    fn classify_assistant_reasoning_pure_assistant_context() {
        let ctx = "[assistant]: Let me think about this step by step.\n\
                   [assistant]: First, we need to consider the constraints.";
        assert_eq!(classify_failure_category(ctx), "assistant_reasoning");
    }

    #[test]
    fn classify_user_context_dominant_user_turns() {
        let ctx = "[user]: what is X?\n[user]: and also Y?\n[user]: and Z?\n[assistant]: ...";
        assert_eq!(classify_failure_category(ctx), "user_context");
    }

    #[test]
    fn classify_tool_output_wins_over_user_markers() {
        let ctx = "[user]: please run the command\n[tool output]: exit 0";
        assert_eq!(
            classify_failure_category(ctx),
            "tool_output",
            "tool_output must take priority when both markers are present"
        );
    }

    #[test]
    fn classify_unknown_for_empty_context() {
        assert_eq!(classify_failure_category(""), "unknown");
    }

    #[test]
    fn classify_unknown_for_context_without_markers() {
        let ctx = "This is a generic summary without any role markers or tool output.";
        assert_eq!(classify_failure_category(ctx), "unknown");
    }
}
