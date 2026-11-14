// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;

/// Maximum input size for embedding APIs, expressed in UTF-8 bytes.
///
/// At a conservative 4 bytes/token heuristic this maps to ~8 000 tokens, staying
/// within the `OpenAI` `text-embedding-3-*` limit of 8 191 tokens.
///
/// Note: the byte-based limit is a proxy for token count only. For high-density
/// scripts (CJK, emoji) each character may encode more than one token, so
/// `InvalidInput` returned by providers on HTTP 400 serves as the real safety net.
pub(crate) const EMBED_MAX_BYTES: usize = 32_000;
const EMBED_HEAD_BYTES: usize = 24_000;
const EMBED_TAIL_BYTES: usize = 8_000;
const EMBED_TRUNCATION_MARKER: &str = "\n...[truncated]...\n";

/// Convert a slice of string references to owned `String` values.
#[inline]
pub(crate) fn owned_strs(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| (*t).to_owned()).collect()
}

/// Truncates `text` for embedding APIs with a token limit.
///
/// Uses a head+tail strategy: keeps the first [`EMBED_HEAD_BYTES`] and last
/// [`EMBED_TAIL_BYTES`] bytes separated by a truncation marker. Returns
/// a borrowed slice unchanged when the input is within the limit.
///
/// # Overlap guard
///
/// When the text barely exceeds `EMBED_MAX_BYTES` but is shorter than
/// `EMBED_HEAD_BYTES + EMBED_TAIL_BYTES + marker.len()`, the naive split would
/// duplicate content. In that case the text is returned borrowed as-is — it is
/// already within a range that makes splitting counter-productive, and the
/// provider's `InvalidInput` error is the backstop.
pub(crate) fn truncate_for_embed(text: &str) -> Cow<'_, str> {
    if text.len() <= EMBED_MAX_BYTES {
        return Cow::Borrowed(text);
    }

    // Guard: if barely over limit, head+tail ranges would overlap — return as-is.
    if text.len() <= EMBED_HEAD_BYTES + EMBED_TAIL_BYTES + EMBED_TRUNCATION_MARKER.len() {
        return Cow::Borrowed(text);
    }

    // Find safe UTF-8 boundary for the head slice.
    let head = zeph_common::text::truncate_to_bytes_ref(text, EMBED_HEAD_BYTES);

    // Find safe UTF-8 boundary for the tail slice (search from the end).
    let tail_byte_start = text.len().saturating_sub(EMBED_TAIL_BYTES);
    let tail = text
        .get(text.ceil_char_boundary(tail_byte_start)..)
        .unwrap_or_default();

    tracing::warn!(
        original_bytes = text.len(),
        head_bytes = head.len(),
        tail_bytes = tail.len(),
        "embed input truncated to fit provider limit"
    );

    Cow::Owned(format!("{head}{EMBED_TRUNCATION_MARKER}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn empty_string_is_borrowed() {
        let result = truncate_for_embed("");
        assert_matches!(result, Cow::Borrowed(_));
        assert_eq!(result, "");
    }

    #[test]
    fn short_string_is_borrowed() {
        let input = "hello world";
        let result = truncate_for_embed(input);
        assert_matches!(result, Cow::Borrowed(_));
        assert_eq!(result, input);
    }

    #[test]
    fn exactly_at_limit_is_borrowed() {
        let input = "a".repeat(EMBED_MAX_BYTES);
        let result = truncate_for_embed(&input);
        assert_matches!(result, Cow::Borrowed(_));
        assert_eq!(result.len(), EMBED_MAX_BYTES);
    }

    #[test]
    fn over_limit_latin_is_owned_with_head_and_tail() {
        // Make a string well above the limit so head+tail don't overlap.
        let input = "a".repeat(EMBED_MAX_BYTES + 10_000);
        let result = truncate_for_embed(&input);
        assert_matches!(result, Cow::Owned(_));
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
        // which exceeds EMBED_MAX_BYTES (32 000).
        let input = "中".repeat(12_000);
        let result = truncate_for_embed(&input);
        // Must be valid UTF-8 (no panic on access).
        let s: &str = result.as_ref();
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        assert!(s.len() < input.len());
        // The truncation marker must be present — 12 000 CJK chars (36 000 bytes)
        // is well above EMBED_MAX_BYTES and avoids the overlap guard.
        assert!(
            s.contains("...[truncated]..."),
            "truncation marker must appear in CJK output"
        );
    }

    #[test]
    fn overlap_guard_returns_borrowed() {
        // Text length just above EMBED_MAX_BYTES but below the overlap threshold.
        // EMBED_HEAD_BYTES + EMBED_TAIL_BYTES + MARKER.len() = 24_000 + 8_000 + 19 = 32_019
        // So any text between 32_001 and 32_019 bytes triggers the guard.
        let input = "b".repeat(EMBED_MAX_BYTES + 1);
        let result = truncate_for_embed(&input);
        // Overlap guard kicks in — the text is returned borrowed unchanged.
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn overlap_guard_upper_boundary_is_borrowed() {
        // A string of exactly HEAD + TAIL + MARKER bytes is still within the guard
        // (condition uses <=) and must be returned borrowed.
        let boundary = EMBED_HEAD_BYTES + EMBED_TAIL_BYTES + EMBED_TRUNCATION_MARKER.len();
        let input = "c".repeat(boundary);
        let result = truncate_for_embed(&input);
        assert_matches!(
            result,
            Cow::Borrowed(_),
            "text at the overlap guard boundary must be returned borrowed"
        );
    }

    #[test]
    fn one_byte_past_overlap_guard_produces_owned() {
        // One byte past the overlap guard boundary: splitting is now valid.
        let boundary = EMBED_HEAD_BYTES + EMBED_TAIL_BYTES + EMBED_TRUNCATION_MARKER.len();
        let input = "d".repeat(boundary + 1);
        let result = truncate_for_embed(&input);
        assert_matches!(
            result,
            Cow::Owned(_),
            "text one byte past the overlap guard must be truncated (Cow::Owned)"
        );
        assert!(result.contains("...[truncated]..."));
    }

    #[test]
    fn truncated_result_has_correct_structure() {
        // Create a string large enough to avoid the overlap guard.
        let head = "H".repeat(EMBED_HEAD_BYTES);
        let middle = "M".repeat(5_000);
        let tail = "T".repeat(EMBED_TAIL_BYTES);
        let input = format!("{head}{middle}{tail}");

        let result = truncate_for_embed(&input);
        assert_matches!(result, Cow::Owned(_));
        let s = result.as_ref();

        // Head section is present.
        assert!(s.starts_with('H'));
        // Tail section is present.
        assert!(s.ends_with('T'));
        // Middle is dropped.
        assert!(!s.contains('M'));
    }
}
