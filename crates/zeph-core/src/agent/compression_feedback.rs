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
        let config = &self.memory_state.compression_guidelines_config;

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

        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Some(cid) = self.memory_state.conversation_id else {
            return;
        };

        // Store the actual LLM response (failure signal) so the guidelines updater
        // can derive specific rules from it, not the detection metadata string.
        let sqlite = memory.sqlite();
        if let Err(e) = sqlite
            .log_compression_failure(cid, &compressed_context, response)
            .await
        {
            tracing::warn!("failed to log compression failure pair: {e:#}");
        } else {
            tracing::info!(
                turns_since_compaction = turns,
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
        for msg in self.messages.iter().skip(1).take(3) {
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
}
