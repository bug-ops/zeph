// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Failure detection for ACON compression guidelines (#1647).
//!
//! Detects whether an LLM response indicates context loss after compaction,
//! using a conservative two-signal approach to minimize false positives.

use std::sync::LazyLock;

use crate::agent::Agent;
use crate::channel::Channel;

use regex::Regex;

/// Explicit uncertainty phrases — signals the agent is unaware of something.
///
/// Pattern set 1: must match for detection to fire.
static UNCERTAINTY_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // "I don't have access to", "I no longer have information about"
        Regex::new(
            r"(?i)\b(i\s+(don'?t|no\s+longer)\s+have\s+(access\s+to|information\s+(about|on|regarding)))\b",
        )
        .unwrap(),
        // "I wasn't provided with" / "I haven't been given"
        Regex::new(r"(?i)\b(i\s+(wasn'?t|haven'?t\s+been)\s+(provided|given)\s+with)\b").unwrap(),
        // "I don't recall" / "I don't remember" (in context of prior information)
        Regex::new(
            r"(?i)\b(i\s+don'?t\s+(recall|remember)\s+(any|the|what|which)\s+(previous|earlier|prior|specific))\b",
        )
        .unwrap(),
        // "I'm not sure what we discussed" / "I'm not sure what was decided"
        Regex::new(
            r"(?i)\b(i'?m\s+not\s+sure\s+what\s+(we|was|had)\s+(discussed|covered|decided|established))\b",
        )
        .unwrap(),
        // "could you remind me" / "could you tell me again"
        Regex::new(r"(?i)\b(could\s+you\s+(remind|tell)\s+me\s+(what|again|about))\b").unwrap(),
    ]
});

/// Prior-context reference phrases — signals the response references something that
/// should have been in the compressed context.
///
/// Pattern set 2: must ALSO match for detection to fire.
static PRIOR_CONTEXT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // "earlier in our conversation" / "previously in this session"
        Regex::new(
            r"(?i)\b(earlier\s+in\s+(our|this)\s+(conversation|session|chat|discussion))\b",
        )
        .unwrap(),
        // "previously you mentioned" / "before you said"
        Regex::new(
            r"(?i)\b((previously|before)\s+(you\s+(mentioned|said|told|described|shared)|we\s+(discussed|covered|established|decided)))\b",
        )
        .unwrap(),
        // "in our earlier exchange" / "from our prior discussion"
        Regex::new(
            r"(?i)\b(in\s+(our|the)\s+(earlier|previous|prior|past)\s+(exchange|discussion|conversation|messages|context))\b",
        )
        .unwrap(),
        // "based on what you told me earlier" / "based on what you said before"
        Regex::new(
            r"(?i)\b(based\s+on\s+what\s+(you|we)\s+(told|said|discussed|shared)(\s+\w+)?\s+(earlier|before|previously))\b",
        )
        .unwrap(),
    ]
});

/// Classify which content category was likely lost in a compaction failure.
///
/// Classification is performed on the compaction summary text (before LLM summarization
/// destroys original markers), not on the post-summary response. This heuristic works
/// because `compact_context()` builds the summary prefix from the original messages,
/// and the compressed context snapshot captured by `extract_last_compaction_summary()`
/// includes those original message markers.
///
/// Returns one of: `tool_output`, `assistant_reasoning`, `user_context`, `unknown`.
#[must_use]
pub fn classify_failure_category(compressed_context: &str) -> &'static str {
    // Tool output markers appear when the compacted range contained tool results.
    let has_tool_markers = compressed_context.contains("[tool output")
        || compressed_context.contains("ToolOutput")
        || compressed_context.contains("ToolResult")
        || compressed_context.contains("[archived:")
        || compressed_context.contains("read_overflow");

    // User context markers: the compacted range was heavy in user messages.
    let has_user_markers = compressed_context.contains("[user]:") ||
        // Heuristic: high density of "[user]:" relative to "[assistant]:" suggests user context.
        compressed_context
            .matches("[user]:")
            .count()
            .saturating_mul(2)
            > compressed_context.matches("[assistant]:").count();

    // Assistant reasoning: the compacted range was heavy in assistant reasoning.
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
/// This two-signal requirement minimizes false positives compared to a single-pattern
/// approach. Neither pattern set alone is reliable; together they are highly specific.
///
/// # Returns
///
/// - `None` when `had_compaction` is `false` (no-op when feature disabled or no compaction).
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

impl<C: Channel> Agent<C> {
    /// Check the LLM response for signs of context loss after compaction.
    ///
    /// Fires only when:
    /// 1. The feature is enabled in config
    /// 2. A hard compaction has occurred in this session
    /// 3. The number of turns since last compaction is within the detection window
    /// 4. Both uncertainty and prior-context signals are present in the response
    ///
    /// If all conditions are met, logs a failure pair to `SQLite` (non-fatal on error).
    pub(crate) async fn maybe_log_compression_failure(&self, response: &str) {
        let config = &self.memory_state.compaction.compression_guidelines_config;

        // CRITICAL: first check must be `enabled` guard (critic finding 5.2).
        if !config.enabled {
            return;
        }

        // Only watch within the configured detection window after a hard compaction.
        let Some(turns) = self.context_manager.turns_since_last_hard_compaction else {
            return;
        };
        if turns > config.detection_window_turns {
            return;
        }

        let Some(detection_meta) = detect_compression_failure(response, true) else {
            return;
        };

        tracing::debug!(meta = %detection_meta, "compression failure detected");

        // Extract the most recent compaction summary from messages[1..3].
        let compressed_context = self.extract_last_compaction_summary();

        let Some(memory) = &self.memory_state.persistence.memory else {
            return;
        };
        let Some(cid) = self.memory_state.persistence.conversation_id else {
            return;
        };

        // Classify the failure category based on the compaction summary content.
        // Classification happens before calling the LLM (which would destroy original markers).
        let category = classify_failure_category(&compressed_context);

        // Store the actual LLM response (failure signal) so the guidelines updater
        // can derive specific rules from it, not the detection metadata string.
        let sqlite = memory.sqlite();
        if let Err(e) = sqlite
            .log_compression_failure(cid, &compressed_context, response, category)
            .await
        {
            tracing::warn!("failed to log compression failure pair: {e:#}");
        } else {
            tracing::info!(
                turns_since_compaction = turns,
                category,
                "compression failure detected and logged"
            );
        }
    }

    /// Extract the most recent compaction summary text from the message history.
    ///
    /// After `compact_context()`, a `[conversation summary — N messages compacted]`
    /// system message is inserted at index 1. This method scans positions 1..4
    /// to find and return that summary text.
    fn extract_last_compaction_summary(&self) -> String {
        const SUMMARY_MARKER: &str = "[conversation summary";
        for msg in self.msg.messages.iter().skip(1).take(3) {
            if msg.content.starts_with(SUMMARY_MARKER) {
                return msg.content.clone();
            }
        }
        String::new()
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
        // "I don't recall" without a prior-context reference phrase.
        // "previous" appears but not in the required compound PRIOR_CONTEXT_PATTERNS form.
        let response = "I don't recall the specific previous details.";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "uncertainty alone must not fire without a prior-context anchor phrase"
        );
    }

    #[test]
    fn no_detection_normal_conversation_reference() {
        // "as mentioned earlier" is a normal conversational reference, not context loss.
        // It lacks the uncertainty phrase, so should NOT fire.
        let response = "As mentioned earlier, the function takes two arguments.";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "normal conversational reference must not trigger"
        );
    }

    #[test]
    fn no_detection_llm_asking_clarifying_question() {
        // "could you tell me" but not in the context of lost information.
        let response = "Could you tell me more about what you'd like the function to do?";
        assert!(
            detect_compression_failure(response, true).is_none(),
            "clarifying question without prior-context ref must not fire"
        );
    }

    #[test]
    fn no_detection_legitimate_i_dont_see_previous() {
        // LLM saying it doesn't see something the user hasn't provided yet.
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
        // Only assistant turns, no tool or user markers.
        let ctx = "[assistant]: Let me think about this step by step.\n\
                   [assistant]: First, we need to consider the constraints.";
        assert_eq!(classify_failure_category(ctx), "assistant_reasoning");
    }

    #[test]
    fn classify_user_context_dominant_user_turns() {
        // 3 user turns vs 1 assistant turn — user wins the 2:1 ratio heuristic.
        let ctx = "[user]: what is X?\n[user]: and also Y?\n[user]: and Z?\n[assistant]: ...";
        assert_eq!(classify_failure_category(ctx), "user_context");
    }

    #[test]
    fn classify_tool_output_wins_over_user_markers() {
        // Both tool and user markers present — tool takes priority.
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
