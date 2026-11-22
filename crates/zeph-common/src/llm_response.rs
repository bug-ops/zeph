// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers for extracting structured content embedded in free-form LLM prose.
//!
//! LLMs are prompted to return JSON but often wrap it in prose or markdown fences.
//! This module provides the bracket-matching extraction step shared by callers that
//! parse a JSON array out of a raw LLM response; each caller keeps its own policy for
//! what to do when extraction fails (empty-result fallback vs. hard error).

/// Strips a Markdown code fence (with any language tag) from an LLM response.
///
/// LLMs routinely wrap JSON, YAML or Markdown payloads in a fence. This is the single shared
/// unwrapping step; callers parse the returned slice.
///
/// The result borrows from `raw` and is trimmed. Input that does not start with a fence
/// (after trimming) is returned trimmed and otherwise unchanged. After the opening fence an
/// optional tag of `[A-Za-z0-9_.+-]` is read:
///
/// - a `{` or `[` right after the tag (and optional blanks) starts the payload there;
/// - otherwise a multi-line input treats the whole first line as the info string;
/// - otherwise (single line) everything after the opening fence is the payload.
///
/// The closing fence is the last run of three backticks followed only by blanks up to the end
/// of its line; anything after it is dropped. A missing closer takes the rest of the input.
/// Fences of four or more backticks and tilde fences are not detected.
///
/// # Examples
///
/// ```
/// use zeph_common::llm_response::strip_code_fence;
///
/// assert_eq!(strip_code_fence("```json\n{\"a\": 1}\n```\nHope this helps!"), r#"{"a": 1}"#);
/// assert_eq!(strip_code_fence("  plain  "), "plain");
/// ```
#[must_use]
pub fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix(FENCE) else {
        return trimmed;
    };
    strip_closing_fence(fence_body(after_open)).trim()
}

const FENCE: &str = "```";

/// Returns the payload part of `after_open` (the text following the opening fence).
fn fence_body(after_open: &str) -> &str {
    let after_tag = after_open.trim_start_matches(|c: char| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-')
    });
    let payload = after_tag.trim_start_matches([' ', '\t']);
    if payload.starts_with(['{', '[']) {
        return payload;
    }
    after_open
        .split_once('\n')
        .map_or(after_open, |(_, rest)| rest)
}

/// Cuts `body` at its last fence that is followed only by blanks on its line.
fn strip_closing_fence(body: &str) -> &str {
    body.rmatch_indices(FENCE)
        .find_map(|(at, _)| {
            let (head, tail) = body.split_at_checked(at)?;
            let rest = tail.strip_prefix(FENCE)?;
            let line = rest.split_once('\n').map_or(rest, |(line, _)| line);
            line.bytes()
                .all(|b| matches!(b, b' ' | b'\t' | b'\r'))
                .then_some(head)
        })
        .unwrap_or(body)
}

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
