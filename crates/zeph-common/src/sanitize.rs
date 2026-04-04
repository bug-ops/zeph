// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared sanitization primitives.
//!
//! Domain-specific sanitization belongs in the respective crates. This module
//! only provides the shared low-level primitives (control char stripping,
//! null byte removal) that multiple crates need.

/// Strip ASCII control characters (U+0000–U+001F, U+007F) from `s`.
///
/// Also strips Unicode `BiDi` override codepoints (U+202A–U+202E, U+2066–U+2069)
/// which can be used to visually obscure malicious content.
#[must_use]
pub fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c as u32, 0x202A..=0x202E | 0x2066..=0x2069))
        .collect()
}

/// Strip ASCII control characters while preserving common whitespace (`\t`, `\n`, `\r`).
///
/// Also strips Unicode `BiDi` override codepoints (U+202A–U+202E, U+2066–U+2069).
/// Use this variant when the input may contain intentional newlines or tabs that
/// should be kept (e.g., multi-line tool output, webhook payloads).
#[must_use]
pub fn strip_control_chars_preserve_whitespace(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            (!c.is_control() || c == '\t' || c == '\n' || c == '\r')
                && !matches!(c as u32, 0x202A..=0x202E | 0x2066..=0x2069)
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
}
