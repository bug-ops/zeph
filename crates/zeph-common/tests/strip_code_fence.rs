// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests (tests-first) for #7175: the single shared
//! `zeph_common::llm_response::strip_code_fence` that replaces the eight per-crate copies.

use proptest::prelude::*;
use zeph_common::llm_response::strip_code_fence;

const FENCE: &str = "```";

/// Asserts every `(input, expected)` pair, naming the offending input on failure.
fn assert_all(cases: &[(&str, &str)]) {
    for (input, expected) in cases {
        assert_eq!(strip_code_fence(input), *expected, "input: {input:?}");
    }
}

// ---- the seven cases moved from zeph-llm's strip_json_fences ----

#[test]
fn strip_code_fence_plain_json_unchanged() {
    assert_eq!(strip_code_fence(r#"{"a": 1}"#), r#"{"a": 1}"#);
}

#[test]
fn strip_code_fence_json_tag() {
    assert_eq!(strip_code_fence("```json\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
}

#[test]
fn strip_code_fence_untagged() {
    assert_eq!(strip_code_fence("```\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
}

#[test]
fn strip_code_fence_whitespace_only() {
    assert_eq!(strip_code_fence("  \n  "), "");
}

#[test]
fn strip_code_fence_empty() {
    assert_eq!(strip_code_fence(""), "");
}

#[test]
fn strip_code_fence_outer_whitespace() {
    assert_eq!(
        strip_code_fence("  ```json\n{\"a\": 1}\n```  "),
        r#"{"a": 1}"#
    );
}

#[test]
fn strip_code_fence_only_opening_fence() {
    assert_eq!(strip_code_fence("```json\n{\"a\": 1}"), r#"{"a": 1}"#);
}

// ---- language tags ----

#[test]
fn strip_code_fence_any_language_tag_is_dropped() {
    assert_all(&[
        ("```JSON\n{\"a\":1}\n```", r#"{"a":1}"#),
        ("```jsonc\n{\"a\":1}\n```", r#"{"a":1}"#),
        ("```md\n# Title\nbody\n```", "# Title\nbody"),
        ("```yaml\na: 1\nb: 2\n```", "a: 1\nb: 2"),
        ("```c++\nint x;\n```", "int x;"),
    ]);
}

#[test]
fn strip_code_fence_multiline_info_string_is_whole_first_line() {
    assert_eq!(
        strip_code_fence("```json title=\"x\"\n{\"a\":1}\n```"),
        r#"{"a":1}"#
    );
}

// ---- single-line forms ----

/// Known limitation, not a regression (the old copies fail the same way): the whole first
/// line is the info string, so a brace after a multi-word info string is dropped with it.
#[test]
fn strip_code_fence_multi_word_info_string_before_brace_loses_the_brace() {
    assert_eq!(
        strip_code_fence("```json title {\n  \"a\": 1\n}\n```"),
        "\"a\": 1\n}"
    );
}

#[test]
fn strip_code_fence_single_line_with_and_without_tag() {
    assert_all(&[
        ("```json {\"a\":1}```", r#"{"a":1}"#),
        ("```json{\"a\":1}```", r#"{"a":1}"#),
        ("```{\"a\":1}```", r#"{"a":1}"#),
        ("```json [1,2]```", "[1,2]"),
    ]);
}

#[test]
fn strip_code_fence_single_line_prose_keeps_first_word() {
    assert_eq!(strip_code_fence("```hello world```"), "hello world");
}

// ---- payload starting on the opening line (N1) ----

#[test]
fn strip_code_fence_payload_on_opening_line() {
    assert_all(&[
        ("```json {\n  \"a\": 1\n}\n```", "{\n  \"a\": 1\n}"),
        ("```json [\n  1,\n  2\n]\n```", "[\n  1,\n  2\n]"),
        ("```{\n  \"a\": 1\n}\n```", "{\n  \"a\": 1\n}"),
    ]);
}

// ---- closing rules (N2) ----

#[test]
fn strip_code_fence_same_line_close() {
    assert_eq!(strip_code_fence("```json\n{\"a\":1}```"), r#"{"a":1}"#);
}

#[test]
fn strip_code_fence_same_line_close_then_prose_on_next_line() {
    assert_eq!(
        strip_code_fence("```json\n{\"a\":1}```\nHope this helps!"),
        r#"{"a":1}"#
    );
}

#[test]
fn strip_code_fence_own_line_close_then_prose() {
    assert_eq!(
        strip_code_fence("```json\n{\"a\":1}\n```\n\nHope this helps!"),
        r#"{"a":1}"#
    );
}

#[test]
fn strip_code_fence_crlf_with_trailing_prose() {
    assert_eq!(
        strip_code_fence("```json\r\n{\"a\":1}\r\n```\r\nThanks"),
        r#"{"a":1}"#
    );
}

#[test]
fn strip_code_fence_crlf_without_prose() {
    assert_eq!(
        strip_code_fence("```json\r\n{\"a\":1}\r\n```"),
        r#"{"a":1}"#
    );
}

#[test]
fn strip_code_fence_backticks_inside_value_are_not_a_closer() {
    assert_eq!(
        strip_code_fence("```json\n{\"v\": \"x```\"}\n```"),
        "{\"v\": \"x```\"}"
    );
}

#[test]
fn strip_code_fence_inner_block_with_outer_close_is_kept() {
    assert_eq!(
        strip_code_fence("```md\nintro\n```rust\ncode\n```\noutro\n```"),
        "intro\n```rust\ncode\n```\noutro"
    );
}

#[test]
fn strip_code_fence_unterminated_outer_cuts_at_inner_closer() {
    assert_eq!(
        strip_code_fence("```md\nintro\n```rust\nfn main() {}\n```"),
        "intro\n```rust\nfn main() {}"
    );
}

#[test]
fn strip_code_fence_tilde_fence_is_not_detected() {
    assert_eq!(
        strip_code_fence("~~~json\n{\"a\":1}\n~~~"),
        "~~~json\n{\"a\":1}\n~~~"
    );
}

// ---- fence-free input and non-ASCII ----

#[test]
fn strip_code_fence_fence_free_input_is_only_trimmed() {
    assert_all(&[
        ("  hello `code` world \n", "hello `code` world"),
        ("see ```x```", "see ```x```"),
    ]);
}

#[test]
fn strip_code_fence_non_ascii() {
    assert_all(&[
        (
            "```json\n{\"имя\": \"значение ✓\"}\n```",
            "{\"имя\": \"значение ✓\"}",
        ),
        ("```héllo wörld```", "héllo wörld"),
        ("```日本\nx\n```", "x"),
        ("```😀```", "😀"),
    ]);
}

// ---- properties ----

proptest! {
    #[test]
    fn strip_code_fence_never_panics_and_borrows_from_input(raw in "[`a-z{}\\[\\]\n\r \t\"é✓]{0,80}") {
        let out = strip_code_fence(&raw);
        prop_assert!(raw.contains(out));
    }

    #[test]
    fn strip_code_fence_arbitrary_unicode_never_panics(raw in "\\PC*") {
        let _ = strip_code_fence(&raw);
    }

    #[test]
    fn strip_code_fence_fence_free_input_equals_trim(raw in "[a-z{}\\[\\]\n \t`\"]{0,60}") {
        prop_assume!(!raw.contains(FENCE));
        prop_assert_eq!(strip_code_fence(&raw), raw.trim());
    }

    #[test]
    fn strip_code_fence_round_trips_fenced_payload(
        tag in prop::sample::select(vec!["", "json", "JSON", "jsonc", "md", "yaml"]),
        body in "[a-z0-9{}\\[\\]\", :\n]{0,40}",
    ) {
        let fenced = format!("{FENCE}{tag}\n{body}\n{FENCE}");
        prop_assert_eq!(strip_code_fence(&fenced), body.trim());
    }
}
