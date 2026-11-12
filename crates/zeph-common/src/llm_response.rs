// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers for extracting structured content embedded in free-form LLM prose.
//!
//! LLMs are prompted to return JSON but often wrap it in prose or markdown fences.
//! This module provides the bracket-matching extraction step shared by callers that
//! parse a JSON array out of a raw LLM response; each caller keeps its own policy for
//! what to do when extraction fails (empty-result fallback vs. hard error).

/// Extract the substring spanning the first `[` and the last `]` in `raw`.
///
/// Returns `None` when `raw` has no `[`, no `]` after it, or when the `]` does not come
/// after the `[` (e.g. `"] before ["`). Does not validate that the slice is valid JSON —
/// callers must still attempt to parse the returned slice.
///
/// # Examples
///
/// ```
/// use zeph_common::llm_response::extract_json_array_slice;
///
/// assert_eq!(
///     extract_json_array_slice("Sure, here you go: [1, 2, 3] — hope that helps!"),
///     Some("[1, 2, 3]")
/// );
/// assert_eq!(extract_json_array_slice("no brackets here"), None);
/// ```
#[must_use]
pub fn extract_json_array_slice(raw: &str) -> Option<&str> {
    extract_delimited(raw, '[', ']')
}

/// Extract the substring spanning the first `{` and the last `}` in `raw`.
///
/// Returns `None` when `raw` has no `{`, no `}` after it, or when the `}` does not come
/// after the `{` (e.g. `"no} verdict {"`). Never panics on non-ASCII input. Does not
/// validate that the slice is valid JSON — callers must still attempt to parse it.
///
/// # Examples
///
/// ```
/// use zeph_common::llm_response::extract_json_object_slice;
///
/// assert_eq!(
///     extract_json_object_slice("Verdict: {\"ok\": true} done"),
///     Some("{\"ok\": true}")
/// );
/// assert_eq!(extract_json_object_slice("no} verdict {"), None);
/// ```
#[must_use]
pub fn extract_json_object_slice(raw: &str) -> Option<&str> {
    extract_delimited(raw, '{', '}')
}

fn extract_delimited(raw: &str, open: char, close: char) -> Option<&str> {
    let start = raw.find(open)?;
    // Search only the suffix from `start`, so a stray `close` before any `open`
    // can never produce an inverted range.
    let suffix = raw.get(start..)?;
    let end = start + suffix.rfind(close)? + close.len_utf8();
    raw.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_array_wrapped_in_prose() {
        assert_eq!(
            extract_json_array_slice("Sure! [1, 2, 3] there you go."),
            Some("[1, 2, 3]")
        );
    }

    #[test]
    fn extracts_bare_array() {
        assert_eq!(extract_json_array_slice("[]"), Some("[]"));
    }

    #[test]
    fn none_when_no_open_bracket() {
        assert_eq!(extract_json_array_slice("no brackets"), None);
    }

    #[test]
    fn none_when_no_close_bracket_after_open() {
        assert_eq!(extract_json_array_slice("] then ["), None);
    }

    #[test]
    fn none_when_only_close_bracket() {
        assert_eq!(extract_json_array_slice("no open bracket]"), None);
    }

    #[test]
    fn object_extracted_from_prose() {
        assert_eq!(
            extract_json_object_slice("Sure: {\"a\": 1} ok"),
            Some("{\"a\": 1}")
        );
    }

    #[test]
    fn object_none_when_close_precedes_open() {
        assert_eq!(extract_json_object_slice("no} verdict {"), None);
        assert_eq!(extract_json_object_slice("} x {"), None);
    }

    #[test]
    fn object_none_without_braces() {
        assert_eq!(extract_json_object_slice("plain"), None);
        assert_eq!(extract_json_object_slice("only {"), None);
    }

    #[test]
    fn object_handles_non_ascii_around_braces() {
        assert_eq!(
            extract_json_object_slice("ответ: {\"k\": \"значение\"} готово"),
            Some("{\"k\": \"значение\"}")
        );
        assert_eq!(extract_json_object_slice("é} вердикт {é"), None);
    }
}
