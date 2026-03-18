// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! String utility functions for Unicode-safe text manipulation.

/// Truncate `s` to at most `max_bytes` bytes, preserving UTF-8 char boundaries.
///
/// Returns an owned `String`. If `s` fits within `max_bytes`, returns a copy
/// unchanged. Otherwise, walks char boundaries and truncates at the largest
/// boundary that fits.
#[must_use]
pub fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut byte_count = 0usize;
    let mut end = 0usize;
    for ch in s.chars() {
        let ch_len = ch.len_utf8();
        if byte_count + ch_len > max_bytes {
            break;
        }
        byte_count += ch_len;
        end += ch_len;
    }
    s[..end].to_owned()
}

/// Borrow a prefix of `s` that fits within `max_bytes` bytes.
///
/// Returns a subslice of `s`. Walks backwards from `max_bytes` to find a valid
/// UTF-8 char boundary.
#[must_use]
pub fn truncate_to_bytes_ref(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Borrow a prefix of `s` that is at most `max_chars` Unicode scalar values long.
///
/// Returns a subslice of `s`. No ellipsis is appended.
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

    // truncate_to_bytes tests
    #[test]
    fn bytes_short_unchanged() {
        assert_eq!(truncate_to_bytes("hello", 10), "hello");
    }

    #[test]
    fn bytes_exact_unchanged() {
        assert_eq!(truncate_to_bytes("hello", 5), "hello");
    }

    #[test]
    fn bytes_truncates_at_boundary() {
        let s = "hello world";
        assert_eq!(truncate_to_bytes(s, 5), "hello");
    }

    #[test]
    fn bytes_unicode_boundary() {
        // "é" is 2 bytes in UTF-8
        let s = "héllo";
        assert_eq!(truncate_to_bytes(s, 3), "hé");
    }

    #[test]
    fn bytes_zero_returns_empty() {
        assert_eq!(truncate_to_bytes("hello", 0), "");
    }

    // truncate_to_bytes_ref tests
    #[test]
    fn bytes_ref_short_unchanged() {
        assert_eq!(truncate_to_bytes_ref("hello", 10), "hello");
    }

    #[test]
    fn bytes_ref_truncates_at_boundary() {
        assert_eq!(truncate_to_bytes_ref("hello world", 5), "hello");
    }

    #[test]
    fn bytes_ref_unicode_boundary() {
        let s = "héllo";
        assert_eq!(truncate_to_bytes_ref(s, 2), "h");
    }

    // truncate_chars tests
    #[test]
    fn chars_short_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn chars_exact_unchanged() {
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn chars_truncates_by_char() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn chars_zero_returns_empty() {
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn chars_unicode_by_char() {
        let s = "😀😁😂😃😄extra";
        assert_eq!(truncate_chars(s, 5), "😀😁😂😃😄");
    }

    // truncate_to_chars tests
    #[test]
    fn to_chars_short_unchanged() {
        assert_eq!(truncate_to_chars("hello", 10), "hello");
    }

    #[test]
    fn to_chars_exact_unchanged() {
        assert_eq!(truncate_to_chars("hello", 5), "hello");
    }

    #[test]
    fn to_chars_appends_ellipsis() {
        assert_eq!(truncate_to_chars("hello world", 5), "hello\u{2026}");
    }

    #[test]
    fn to_chars_zero_returns_empty() {
        assert_eq!(truncate_to_chars("hello", 0), "");
    }

    #[test]
    fn to_chars_unicode() {
        let s = "😀😁😂😃😄extra";
        assert_eq!(truncate_to_chars(s, 5), "😀😁😂😃😄\u{2026}");
    }
}
