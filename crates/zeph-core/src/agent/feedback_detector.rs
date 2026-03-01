// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Implicit correction detection from user messages.

use std::sync::LazyLock;

use regex::Regex;

static EXPLICIT_REJECTION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)^(no|nope|wrong|incorrect|that'?s\s+not\s+(right|correct|what\s+i))")
            .unwrap(),
        Regex::new(r"(?i)\b(don'?t|do\s+not|stop|quit)\s+(do|doing|use|using)\b").unwrap(),
        Regex::new(r"(?i)\bthat\s+(didn'?t|does\s*n'?t|won'?t)\s+work\b").unwrap(),
        Regex::new(r"(?i)\b(bad|terrible|useless|broken)\s+(answer|response|output|result)\b")
            .unwrap(),
    ]
});

static ALTERNATIVE_REQUEST_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)^(instead|rather|actually|try|use)\b").unwrap(),
        Regex::new(r"(?i)\b(instead\s+of|rather\s+than|not\s+that[,.]?\s+(try|use))\b").unwrap(),
        Regex::new(r"(?i)\b(different|another|alternative)\s+(approach|way|method|solution)\b")
            .unwrap(),
        Regex::new(r"(?i)\bcan\s+you\s+(try|do)\s+it\s+(differently|another\s+way)\b").unwrap(),
    ]
});

/// Classification of a detected correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionKind {
    ExplicitRejection,
    AlternativeRequest,
    Repetition,
    /// Deferred to Phase 3 — requires session-level state machine.
    #[allow(dead_code)]
    Abandonment,
}

impl CorrectionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitRejection => "explicit_rejection",
            Self::AlternativeRequest => "alternative_request",
            Self::Repetition => "repetition",
            Self::Abandonment => "abandonment",
        }
    }
}

/// A detected correction signal from the user.
#[derive(Debug, Clone)]
pub struct CorrectionSignal {
    pub confidence: f32,
    pub kind: CorrectionKind,
    pub feedback_text: String,
}

/// Detects implicit corrections in user messages without an LLM call.
pub struct FeedbackDetector {
    confidence_threshold: f32,
}

impl FeedbackDetector {
    #[must_use]
    pub fn new(confidence_threshold: f32) -> Self {
        Self {
            confidence_threshold,
        }
    }

    /// Analyze `user_message` against recent conversation context.
    ///
    /// `previous_messages` should be user-role messages in chronological order.
    /// Returns `Some(signal)` when a correction is detected above the threshold.
    #[must_use]
    pub fn detect(
        &self,
        user_message: &str,
        previous_messages: &[&str],
    ) -> Option<CorrectionSignal> {
        if let Some(signal) = Self::check_explicit_rejection(user_message)
            && signal.confidence >= self.confidence_threshold
        {
            return Some(signal);
        }
        if let Some(signal) = Self::check_alternative_request(user_message)
            && signal.confidence >= self.confidence_threshold
        {
            return Some(signal);
        }
        if let Some(signal) = Self::check_repetition(user_message, previous_messages)
            && signal.confidence >= self.confidence_threshold
        {
            return Some(signal);
        }
        None
    }

    fn check_explicit_rejection(msg: &str) -> Option<CorrectionSignal> {
        for pattern in EXPLICIT_REJECTION_PATTERNS.iter() {
            if pattern.is_match(msg) {
                return Some(CorrectionSignal {
                    confidence: 0.85,
                    kind: CorrectionKind::ExplicitRejection,
                    feedback_text: msg.to_owned(),
                });
            }
        }
        None
    }

    fn check_alternative_request(msg: &str) -> Option<CorrectionSignal> {
        for pattern in ALTERNATIVE_REQUEST_PATTERNS.iter() {
            if pattern.is_match(msg) {
                return Some(CorrectionSignal {
                    confidence: 0.70,
                    kind: CorrectionKind::AlternativeRequest,
                    feedback_text: msg.to_owned(),
                });
            }
        }
        None
    }

    fn check_repetition(msg: &str, previous_messages: &[&str]) -> Option<CorrectionSignal> {
        let normalized = msg.trim().to_lowercase();
        for prev in previous_messages.iter().rev().take(3) {
            let prev_normalized = prev.trim().to_lowercase();
            if token_overlap(&normalized, &prev_normalized) > 0.8 {
                return Some(CorrectionSignal {
                    confidence: 0.75,
                    kind: CorrectionKind::Repetition,
                    feedback_text: msg.to_owned(),
                });
            }
        }
        None
    }
}

fn token_overlap(a: &str, b: &str) -> f32 {
    let a_tokens: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let b_tokens: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let intersection = a_tokens.intersection(&b_tokens).count() as f32;
    #[allow(clippy::cast_precision_loss)]
    let union = a_tokens.union(&b_tokens).count() as f32;
    intersection / union
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> FeedbackDetector {
        FeedbackDetector::new(0.6)
    }

    #[test]
    fn detect_returns_none_for_normal_message() {
        let d = detector();
        assert!(d.detect("please list all files", &[]).is_none());
        assert!(d.detect("what is 2+2?", &[]).is_none());
        assert!(d.detect("show me the git log", &[]).is_none());
    }

    #[test]
    fn detect_explicit_rejection_no() {
        let d = detector();
        let signal = d.detect("no that's wrong", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
        assert!(signal.confidence >= 0.6);
    }

    #[test]
    fn detect_explicit_rejection_nope() {
        let d = detector();
        let signal = d.detect("nope", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
    }

    #[test]
    fn detect_explicit_rejection_that_didnt_work() {
        let d = detector();
        let signal = d.detect("that didn't work at all", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
    }

    #[test]
    fn detect_alternative_request_instead() {
        let d = detector();
        let signal = d.detect("instead use git rebase", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::AlternativeRequest);
        assert!(signal.confidence >= 0.6);
    }

    #[test]
    fn detect_alternative_request_try() {
        let d = detector();
        let signal = d.detect("try a different approach", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::AlternativeRequest);
    }

    #[test]
    fn detect_repetition_same_message() {
        let d = detector();
        let prev = vec!["list all files in the repo"];
        let signal = d.detect("list all files in the repo", &prev).unwrap();
        assert_eq!(signal.kind, CorrectionKind::Repetition);
    }

    #[test]
    fn detect_repetition_high_overlap() {
        let d = detector();
        let prev = vec!["show me the git log for main branch"];
        let signal = d
            .detect("show me the git log for main branch please", &prev)
            .unwrap();
        assert_eq!(signal.kind, CorrectionKind::Repetition);
    }

    #[test]
    fn detect_no_repetition_different_message() {
        let d = detector();
        let prev = vec!["list files"];
        assert!(d.detect("run the tests", &prev).is_none());
    }

    #[test]
    fn confidence_threshold_filters_low_confidence() {
        // AlternativeRequest fires at 0.70 — threshold 0.8 should suppress it
        let d = FeedbackDetector::new(0.80);
        assert!(d.detect("instead use git rebase", &[]).is_none());
    }

    #[test]
    fn token_overlap_identical() {
        assert!((token_overlap("hello world", "hello world") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn token_overlap_disjoint() {
        assert!((token_overlap("foo bar", "baz qux") - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn token_overlap_empty_a() {
        assert!((token_overlap("", "foo") - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn token_overlap_empty_both() {
        assert!((token_overlap("", "") - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn correction_kind_as_str() {
        assert_eq!(
            CorrectionKind::ExplicitRejection.as_str(),
            "explicit_rejection"
        );
        assert_eq!(
            CorrectionKind::AlternativeRequest.as_str(),
            "alternative_request"
        );
        assert_eq!(CorrectionKind::Repetition.as_str(), "repetition");
        assert_eq!(CorrectionKind::Abandonment.as_str(), "abandonment");
    }

    #[test]
    fn detect_explicit_rejection_dont_do() {
        let d = detector();
        let signal = d.detect("don't do that again", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
    }

    #[test]
    fn detect_explicit_rejection_bad_answer() {
        let d = detector();
        let signal = d.detect("bad answer, try again", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
    }

    #[test]
    fn detect_alternative_request_rather_than() {
        let d = detector();
        let signal = d
            .detect("rather than git merge, use git rebase", &[])
            .unwrap();
        assert_eq!(signal.kind, CorrectionKind::AlternativeRequest);
    }

    #[test]
    fn detect_alternative_request_can_you_try_differently() {
        let d = detector();
        let signal = d.detect("can you try it differently", &[]).unwrap();
        assert_eq!(signal.kind, CorrectionKind::AlternativeRequest);
    }

    #[test]
    fn detect_repetition_empty_previous_messages() {
        let d = detector();
        // no previous messages — should not detect repetition, no panic
        assert!(d.detect("list all files", &[]).is_none());
    }

    #[test]
    fn detect_repetition_only_checks_last_three() {
        let d = detector();
        // identical message at position 4 (beyond the 3-message window) should not trigger
        let prev = vec![
            "list all files in the repo", // position 4 (oldest, beyond window)
            "run the tests",
            "show me the diff",
            "build the project",
        ];
        // "list all files in the repo" is beyond the 3-message window (rev().take(3) gives last 3)
        assert!(d.detect("list all files in the repo", &prev).is_none());
    }

    #[test]
    fn confidence_threshold_blocks_repetition() {
        // Repetition fires at 0.75; threshold 0.80 should suppress it
        let d = FeedbackDetector::new(0.80);
        let prev = vec!["list all files in the repo"];
        assert!(d.detect("list all files in the repo", &prev).is_none());
    }

    #[test]
    fn token_overlap_partial() {
        let overlap = token_overlap("hello world foo", "hello world bar");
        // intersection = {hello, world} = 2; union = {hello, world, foo, bar} = 4 → 0.5
        assert!((overlap - 0.5).abs() < f32::EPSILON);
    }
}
