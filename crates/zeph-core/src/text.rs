// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! String utility functions for Unicode-safe text truncation.

/// Borrow a prefix of `s` that is at most `max_chars` Unicode scalar values long.
///
/// Returns a subslice of `s`. No ellipsis is appended. Use `truncate_to_chars` if
/// you need an owned `String` with a trailing ellipsis on truncation.
#[must_use]
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Truncate a string to at most `max_chars` Unicode scalar values.
///
/// If the string is longer than `max_chars` chars, the first `max_chars` chars are
/// kept and the Unicode ellipsis character `…` (U+2026) is appended. If `max_chars`
/// is zero, returns an empty string.
#[must_use]
pub fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_unchanged() {
        assert_eq!(truncate_to_chars("hello", 10), "hello");
    }

    #[test]
    fn exact_length_unchanged() {
        assert_eq!(truncate_to_chars("hello", 5), "hello");
    }

    #[test]
    fn appends_ellipsis() {
        assert_eq!(truncate_to_chars("hello world", 5), "hello\u{2026}");
    }

    #[test]
    fn zero_max_returns_empty() {
        assert_eq!(truncate_to_chars("hello", 0), "");
    }

    #[test]
    fn unicode_handled_correctly() {
        let s = "😀😁😂😃😄extra";
        assert_eq!(truncate_to_chars(s, 5), "😀😁😂😃😄\u{2026}");
    }

    // truncate_chars (borrow version) tests
    #[test]
    fn borrow_short_string_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn borrow_exact_length_unchanged() {
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn borrow_truncates_to_byte_boundary() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn borrow_zero_max_returns_empty() {
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn borrow_unicode_truncates_by_char() {
        let s = "😀😁😂😃😄extra";
        assert_eq!(truncate_chars(s, 5), "😀😁😂😃😄");
    }

    #[test]
    fn borrow_empty_string() {
        assert_eq!(truncate_chars("", 10), "");
    }
}
