// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;

/// Maximum input size for embedding APIs, expressed in Unicode scalar values.
///
/// At a conservative 4 chars/token heuristic this maps to ~8 000 tokens, staying
/// within the `OpenAI` `text-embedding-3-*` limit of 8 191 tokens.
///
/// Note: the char-based limit is a proxy for token count only. For high-density
/// scripts (CJK, emoji) each character may encode more than one token, so
/// `InvalidInput` returned by providers on HTTP 400 serves as the real safety net.
pub(crate) const EMBED_MAX_CHARS: usize = 32_000;
const EMBED_HEAD_CHARS: usize = 24_000;
const EMBED_TAIL_CHARS: usize = 8_000;
const EMBED_TRUNCATION_MARKER: &str = "\n...[truncated]...\n";

/// Truncates `text` for embedding APIs with a token limit.
///
/// Uses a head+tail strategy: keeps the first [`EMBED_HEAD_CHARS`] and last
/// [`EMBED_TAIL_CHARS`] characters separated by a truncation marker. Returns
/// a borrowed slice unchanged when the input is within the limit.
///
/// # Overlap guard
///
/// When the text barely exceeds `EMBED_MAX_CHARS` but is shorter than
/// `EMBED_HEAD_CHARS + EMBED_TAIL_CHARS + marker.len()`, the naive split would
/// duplicate content. In that case the text is returned borrowed as-is — it is
/// already within a range that makes splitting counter-productive, and the
/// provider's `InvalidInput` error is the backstop.
pub(crate) fn truncate_for_embed(text: &str) -> Cow<'_, str> {
    if text.len() <= EMBED_MAX_CHARS {
        return Cow::Borrowed(text);
    }

    // Guard: if barely over limit, head+tail ranges would overlap — return as-is.
    if text.len() <= EMBED_HEAD_CHARS + EMBED_TAIL_CHARS + EMBED_TRUNCATION_MARKER.len() {
        return Cow::Borrowed(text);
    }

    // Find safe UTF-8 boundary for the head slice.
    let head_end = text.floor_char_boundary(EMBED_HEAD_CHARS);

    // Find safe UTF-8 boundary for the tail slice (search from the end).
    let tail_byte_start = text.len().saturating_sub(EMBED_TAIL_CHARS);
    let tail_start = text.ceil_char_boundary(tail_byte_start);

    tracing::warn!(
        original_chars = text.len(),
        head_chars = head_end,
        tail_chars = text.len() - tail_start,
        "embed input truncated to fit provider limit"
    );

    Cow::Owned(format!(
        "{}{}{}",
        &text[..head_end],
        EMBED_TRUNCATION_MARKER,
        &text[tail_start..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_borrowed() {
        let result = truncate_for_embed("");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "");
    }

    #[test]
    fn short_string_is_borrowed() {
        let input = "hello world";
        let result = truncate_for_embed(input);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, input);
    }

    #[test]
    fn exactly_at_limit_is_borrowed() {
        let input = "a".repeat(EMBED_MAX_CHARS);
        let result = truncate_for_embed(&input);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.len(), EMBED_MAX_CHARS);
    }

    #[test]
    fn over_limit_latin_is_owned_with_head_and_tail() {
        // Make a string well above the limit so head+tail don't overlap.
        let input = "a".repeat(EMBED_MAX_CHARS + 10_000);
        let result = truncate_for_embed(&input);
        assert!(matches!(result, Cow::Owned(_)));
        let s = result.as_ref();
        assert!(s.contains("...[truncated]..."), "marker must be present");
        // Result must be shorter than the original.
        assert!(s.len() < input.len());
        // Head is preserved.
        assert!(s.starts_with(&"a".repeat(100)));
        // Tail is preserved.
        assert!(s.ends_with(&"a".repeat(100)));
    }

    #[test]
    fn over_limit_utf8_multibyte_is_valid_utf8() {
        // CJK characters are 3 bytes each in UTF-8; 12 000 of them = 36 000 bytes
        // which exceeds EMBED_MAX_CHARS (32 000).
        let input = "中".repeat(12_000);
        let result = truncate_for_embed(&input);
        // Must be valid UTF-8 (no panic on access).
        let s: &str = result.as_ref();
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        assert!(s.len() < input.len());
        // The truncation marker must be present — 12 000 CJK chars (36 000 bytes)
        // is well above EMBED_MAX_CHARS and avoids the overlap guard.
        assert!(
            s.contains("...[truncated]..."),
            "truncation marker must appear in CJK output"
        );
    }

    #[test]
    fn overlap_guard_returns_borrowed() {
        // Text length just above EMBED_MAX_CHARS but below the overlap threshold.
        // EMBED_HEAD_CHARS + EMBED_TAIL_CHARS + MARKER.len() = 24_000 + 8_000 + 19 = 32_019
        // So any text between 32_001 and 32_019 chars triggers the guard.
        let input = "b".repeat(EMBED_MAX_CHARS + 1);
        let result = truncate_for_embed(&input);
        // Overlap guard kicks in — the text is returned borrowed unchanged.
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn overlap_guard_upper_boundary_is_borrowed() {
        // A string of exactly HEAD + TAIL + MARKER bytes is still within the guard
        // (condition uses <=) and must be returned borrowed.
        let boundary = EMBED_HEAD_CHARS + EMBED_TAIL_CHARS + EMBED_TRUNCATION_MARKER.len();
        let input = "c".repeat(boundary);
        let result = truncate_for_embed(&input);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "text at the overlap guard boundary must be returned borrowed"
        );
    }

    #[test]
    fn one_byte_past_overlap_guard_produces_owned() {
        // One byte past the overlap guard boundary: splitting is now valid.
        let boundary = EMBED_HEAD_CHARS + EMBED_TAIL_CHARS + EMBED_TRUNCATION_MARKER.len();
        let input = "d".repeat(boundary + 1);
        let result = truncate_for_embed(&input);
        assert!(
            matches!(result, Cow::Owned(_)),
            "text one byte past the overlap guard must be truncated (Cow::Owned)"
        );
        assert!(result.contains("...[truncated]..."));
    }

    #[test]
    fn truncated_result_has_correct_structure() {
        // Create a string large enough to avoid the overlap guard.
        let head = "H".repeat(EMBED_HEAD_CHARS);
        let middle = "M".repeat(5_000);
        let tail = "T".repeat(EMBED_TAIL_CHARS);
        let input = format!("{head}{middle}{tail}");

        let result = truncate_for_embed(&input);
        assert!(matches!(result, Cow::Owned(_)));
        let s = result.as_ref();

        // Head section is present.
        assert!(s.starts_with('H'));
        // Tail section is present.
        assert!(s.ends_with('T'));
        // Middle is dropped.
        assert!(!s.contains('M'));
    }
}
