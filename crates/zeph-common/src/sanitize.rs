// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared sanitization primitives.
//!
//! Domain-specific sanitization belongs in the respective crates. This module
//! only provides the shared low-level primitives (control char stripping,
//! null byte removal) that multiple crates need.

/// Abstraction for sanitizing untrusted task output before LLM injection.
///
/// `zeph-orchestration` uses this trait to avoid a direct dependency on
/// `zeph-sanitizer`. Callers in `zeph-core` supply a concrete implementation.
pub trait OutputSanitizer: Send + Sync {
    /// Sanitize a raw task output string and return the safe version.
    fn sanitize_task_output(&self, text: &str) -> String;
}

/// Passthrough implementation that applies no sanitization.
///
/// Used in tests and contexts where a real sanitizer is not available.
pub struct IdentitySanitizer;

impl OutputSanitizer for IdentitySanitizer {
    fn sanitize_task_output(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Strip all Unicode control characters from `s`, plus the shared bypass-codepoint
/// denylist (`BiDi` overrides, zero-width joiners, soft hyphen, BOM, Hangul/Khmer/Mongolian
/// fillers, the Unicode Tags block — see [`crate::patterns::strip_format_chars`] for the
/// full list).
///
/// Use [`strip_control_chars_preserve_whitespace`] instead when the input may contain
/// intentional tabs or newlines that should be kept.
#[must_use]
pub fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && !crate::patterns::is_bypass_codepoint(c))
        .collect()
}

/// Strip ASCII control characters while preserving common whitespace (`\t`, `\n`, `\r`),
/// plus the shared bypass-codepoint denylist (see [`strip_control_chars`] /
/// [`crate::patterns::strip_format_chars`]).
///
/// Use this variant when the input may contain intentional newlines or tabs that
/// should be kept (e.g., multi-line tool output, webhook payloads). Note this preserves
/// `\r` in addition to `\t`/`\n` (unlike [`crate::patterns::strip_format_chars`], which only
/// preserves `\t`/`\n`) — callers that need CRLF line endings intact use this function.
#[must_use]
pub fn strip_control_chars_preserve_whitespace(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            (!c.is_control() || c == '\t' || c == '\n' || c == '\r')
                && !crate::patterns::is_bypass_codepoint(c)
        })
        .collect()
}

/// Remove null bytes (`\0`) from `s`.
#[must_use]
pub fn strip_null_bytes(s: &str) -> String {
    s.chars().filter(|c| *c != '\0').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_chars_removed() {
        let s = "hello\x00\x01\x1f world\x7f";
        assert_eq!(strip_control_chars(s), "hello world");
    }

    #[test]
    fn bidi_overrides_removed() {
        let bidi = "\u{202A}hidden\u{202C}text";
        let result = strip_control_chars(bidi);
        assert!(!result.contains('\u{202A}'));
        assert!(!result.contains('\u{202C}'));
    }

    #[test]
    fn normal_text_unchanged() {
        assert_eq!(strip_control_chars("hello world"), "hello world");
    }

    #[test]
    fn null_bytes_removed() {
        assert_eq!(strip_null_bytes("hel\0lo"), "hello");
    }

    #[test]
    fn null_bytes_empty_string() {
        assert_eq!(strip_null_bytes(""), "");
    }

    // ── #5925: strip_control_chars now shares strip_format_chars's bypass-codepoint set ──

    #[test]
    fn strip_control_chars_removes_zero_width_space() {
        let result = strip_control_chars("ig\u{200B}nore");
        assert!(!result.contains('\u{200B}'));
        assert_eq!(result, "ignore");
    }

    #[test]
    fn strip_control_chars_removes_soft_hyphen() {
        let result = strip_control_chars("nor\u{00AD}mal");
        assert!(!result.contains('\u{00AD}'));
        assert_eq!(result, "normal");
    }

    #[test]
    fn strip_control_chars_removes_bom() {
        let result = strip_control_chars("\u{FEFF}hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn strip_control_chars_removes_hangul_khmer_mongolian_fillers() {
        assert_eq!(strip_control_chars("a\u{115F}b"), "ab");
        assert_eq!(strip_control_chars("a\u{1160}b"), "ab");
        assert_eq!(strip_control_chars("a\u{17B4}b"), "ab");
        assert_eq!(strip_control_chars("a\u{180B}b"), "ab");
    }

    #[test]
    fn strip_control_chars_removes_tags_block() {
        // U+E0041 TAG LATIN SMALL LETTER A — steganographic prompt-injection vector.
        let result = strip_control_chars("safe\u{E0041}text");
        assert!(!result.contains('\u{E0041}'));
        assert_eq!(result, "safetext");
    }

    #[test]
    fn preserve_whitespace_shares_bypass_codepoints_with_strip_format_chars() {
        // Same bypass-codepoint filtering as strip_format_chars, but preserves `\r` too
        // (strip_format_chars only preserves `\t`/`\n`) — see #5925.
        let input = "ig\u{200B}nore\ninstructions\tnow";
        assert_eq!(
            strip_control_chars_preserve_whitespace(input),
            crate::patterns::strip_format_chars(input)
        );
    }

    #[test]
    fn preserve_whitespace_keeps_carriage_return_unlike_strip_format_chars() {
        let input = "line1\r\nline2";
        assert_eq!(
            strip_control_chars_preserve_whitespace(input),
            "line1\r\nline2"
        );
        assert_eq!(crate::patterns::strip_format_chars(input), "line1\nline2");
    }

    #[test]
    fn preserve_whitespace_removes_bypass_codepoints_keeps_newline_tab() {
        let input = "a\u{00AD}b\nc\td\u{FEFF}e";
        let result = strip_control_chars_preserve_whitespace(input);
        assert_eq!(result, "ab\nc\tde");
    }
}
