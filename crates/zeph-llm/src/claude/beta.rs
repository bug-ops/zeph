// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Beta-feature latches and Claude API error classification.
//!
//! Each latchable beta is disabled for the rest of the session the first time the API rejects
//! its header, which makes the disable-and-retry one-shot by construction: once latched, the
//! header is no longer sent and a repeat rejection cannot name it.

use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::StatusCode;

use crate::error::LlmError;

pub(super) const ANTHROPIC_BETA_COMPACT: &str = "compact-2026-01-12";
pub(super) const ANTHROPIC_BETA_THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";

/// Marker of the 400 returned when a thinking block's prefix binding no longer matches.
const BINDING_MISMATCH_MARKER: &str = "bound to a different conversation";

/// Lowercased fragments that must all occur in the 400 for a `role: "system"` message inside
/// `messages[]`.
const MID_SYSTEM_MARKERS: [&str; 3] = ["role", "system", "not supported"];

/// A beta that the provider can switch off at runtime when the API (or a proxy) rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BetaFeature {
    /// `compact_20260112` server-side compaction.
    Compact,
    /// `thinking.block_binding` and `input_transformations`.
    ThinkingBinding,
}

impl BetaFeature {
    pub(super) const fn header(self) -> &'static str {
        match self {
            Self::Compact => ANTHROPIC_BETA_COMPACT,
            Self::ThinkingBinding => ANTHROPIC_BETA_THINKING_BINDING,
        }
    }

    /// The validation error an API answers with when it does not honor this beta's header and
    /// therefore rejects the request field that only exists under it. Anchored to the
    /// top-level field so a nested validation error (`context_management.edits.0.…`) does not
    /// match.
    const fn unknown_field_error(self) -> &'static str {
        match self {
            Self::Compact => "context_management: Extra inputs are not permitted",
            Self::ThinkingBinding => "thinking.block_binding: Extra inputs are not permitted",
        }
    }
}

/// One-shot "rejected by the API" flags, shared by clones of a provider.
#[derive(Debug, Default)]
pub(super) struct BetaLatches {
    compact: AtomicBool,
    thinking_binding: AtomicBool,
    /// The API answered 400 to a `role: "system"` message inside `messages[]`.
    mid_conversation_system: AtomicBool,
}

impl BetaLatches {
    fn flag(&self, feature: BetaFeature) -> &AtomicBool {
        match feature {
            BetaFeature::Compact => &self.compact,
            BetaFeature::ThinkingBinding => &self.thinking_binding,
        }
    }

    pub(super) fn is_rejected(&self, feature: BetaFeature) -> bool {
        self.flag(feature).load(Ordering::Relaxed)
    }

    /// Latch `feature` as rejected. Returns `true` only for the call that flipped it.
    pub(super) fn reject(&self, feature: BetaFeature) -> bool {
        !self.flag(feature).swap(true, Ordering::Relaxed)
    }

    pub(super) fn mid_conversation_system_rejected(&self) -> bool {
        self.mid_conversation_system.load(Ordering::Relaxed)
    }

    /// Latch mid-conversation system messages as unsupported. Returns `true` only for the call
    /// that flipped it.
    pub(super) fn reject_mid_conversation_system(&self) -> bool {
        !self.mid_conversation_system.swap(true, Ordering::Relaxed)
    }
}

/// Whether a 400 body reports that a `role: "system"` message inside `messages[]` is not
/// supported.
pub(super) fn is_mid_conversation_system_rejection(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    MID_SYSTEM_MARKERS.iter().all(|m| lower.contains(m))
}

/// Whether a 400 body reports a thinking block bound to a different conversation prefix.
///
/// Checked before any beta-header latch: when the binding header was absent the message also
/// names `thinking-binding-controls-2026-08-01`, which must not be read as a header rejection.
pub(super) fn is_thinking_binding_mismatch(status: StatusCode, body: &str) -> bool {
    status == StatusCode::BAD_REQUEST && body.contains(BINDING_MISMATCH_MARKER)
}

/// Whether `marker` occurs in `body` as a root-level field path, not as the tail of a longer
/// path such as `messages.0.<marker>`.
fn contains_root_path(body: &str, marker: &str) -> bool {
    body.match_indices(marker).any(|(at, _)| {
        body[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '.'))
    })
}

/// Whether a 400 body names `feature`'s header as unacceptable, or reports its request field as
/// an unknown extra input (the header was not honored).
pub(super) fn names_rejected_header(status: StatusCode, body: &str, feature: BetaFeature) -> bool {
    status == StatusCode::BAD_REQUEST
        && !is_thinking_binding_mismatch(status, body)
        && (body.contains(feature.header())
            || contains_root_path(body, feature.unknown_field_error()))
}

/// Map a non-success Claude response to an [`LlmError`], recognising a binding mismatch.
pub(super) fn claude_error(status: StatusCode, body: &str) -> LlmError {
    if is_thinking_binding_mismatch(status, body) {
        LlmError::ThinkingBindingMismatch
    } else {
        crate::http::map_error_response(status, body, "claude")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MISMATCH_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\". That setting requires the `thinking-binding-controls-2026-08-01` value in the `anthropic-beta` header."}}"#;

    #[test]
    fn latch_flips_once() {
        let latches = BetaLatches::default();
        assert!(!latches.is_rejected(BetaFeature::Compact));
        assert!(latches.reject(BetaFeature::Compact));
        assert!(!latches.reject(BetaFeature::Compact));
        assert!(latches.is_rejected(BetaFeature::Compact));
        assert!(!latches.is_rejected(BetaFeature::ThinkingBinding));
    }

    #[test]
    fn mismatch_body_is_not_a_header_rejection() {
        let status = StatusCode::BAD_REQUEST;
        assert!(is_thinking_binding_mismatch(status, MISMATCH_BODY));
        assert!(!names_rejected_header(
            status,
            MISMATCH_BODY,
            BetaFeature::ThinkingBinding
        ));
        assert!(matches!(
            claude_error(status, MISMATCH_BODY),
            LlmError::ThinkingBindingMismatch
        ));
    }

    #[test]
    fn header_naming_400_is_a_rejection() {
        let body = "Unexpected value(s) `compact-2026-01-12` for the `anthropic-beta` header.";
        assert!(names_rejected_header(
            StatusCode::BAD_REQUEST,
            body,
            BetaFeature::Compact
        ));
        assert!(!names_rejected_header(
            StatusCode::BAD_REQUEST,
            body,
            BetaFeature::ThinkingBinding
        ));
    }

    #[test]
    fn context_management_validation_error_is_not_a_rejection() {
        let body = "context_management.edits.0.trigger.value: must be at least 50000";
        assert!(!names_rejected_header(
            StatusCode::BAD_REQUEST,
            body,
            BetaFeature::Compact
        ));
    }

    #[test]
    fn extra_inputs_error_for_the_betas_field_is_a_rejection() {
        let status = StatusCode::BAD_REQUEST;
        let body = "thinking.block_binding: Extra inputs are not permitted";
        assert!(names_rejected_header(
            status,
            body,
            BetaFeature::ThinkingBinding
        ));
        assert!(!names_rejected_header(status, body, BetaFeature::Compact));
        let body = "context_management: Extra inputs are not permitted";
        assert!(names_rejected_header(status, body, BetaFeature::Compact));
        let wrapped =
            r#"{"error":{"message":"context_management: Extra inputs are not permitted"}}"#;
        assert!(names_rejected_header(status, wrapped, BetaFeature::Compact));
        for unrelated in [
            "temperature: Extra inputs are not permitted",
            "context_management.edits.0.trigger: Extra inputs are not permitted",
            "messages.0.context_management: Extra inputs are not permitted",
        ] {
            assert!(
                !names_rejected_header(status, unrelated, BetaFeature::Compact),
                "{unrelated}"
            );
        }
    }

    #[test]
    fn mid_conversation_system_rejection_detection() {
        let status = StatusCode::BAD_REQUEST;
        let body =
            r#"{"error":{"message":"messages.3: role 'system' is not supported for this model"}}"#;
        assert!(is_mid_conversation_system_rejection(status, body));
        assert!(!is_mid_conversation_system_rejection(
            StatusCode::INTERNAL_SERVER_ERROR,
            body
        ));
        assert!(!is_mid_conversation_system_rejection(
            status,
            "temperature: not supported"
        ));
        let latches = BetaLatches::default();
        assert!(!latches.mid_conversation_system_rejected());
        assert!(latches.reject_mid_conversation_system());
        assert!(!latches.reject_mid_conversation_system());
        assert!(latches.mid_conversation_system_rejected());
    }

    #[test]
    fn non_400_is_never_a_rejection() {
        assert!(!names_rejected_header(
            StatusCode::INTERNAL_SERVER_ERROR,
            "compact-2026-01-12",
            BetaFeature::Compact
        ));
    }
}
