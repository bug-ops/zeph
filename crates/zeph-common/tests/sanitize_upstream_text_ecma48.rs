// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7167: `sanitize_upstream_text` redacts shapes that are split by a
//! whitespace control or adjacent to any ECMA-48 sequence, and keeps ordinary text readable.

use zeph_common::sanitize::strip_control_chars;
use zeph_common::secret_redact::sanitize_upstream_text;

fn asia() -> String {
    format!("{}{}", "ASIA", "ABCDEFGHIJKLMNOP")
}

fn clean(input: &str) -> String {
    let out = sanitize_upstream_text(input, 500);
    assert_eq!(out, strip_control_chars(&out), "control left: {out:?}");
    assert!(!out.contains('\u{2028}'), "U+2028 left: {out:?}");
    out
}

const SEQUENCES: &[&str] = &[
    "\x1b[31m",
    "\x1b[0;1;38;5;196m",
    "\u{9b}31m",
    "\u{9b}",
    "\x1b(B",
    "\x1b)0",
    "\x1bM",
    "\x1b7",
    "\x1b]0;title\x07",
    "\x1b]8;;http://example.test\x1b\\",
    "\x1bPdcs-payload\x1b\\",
    "\x07",
    "\n",
    "\t",
    "\r\n",
];

#[test]
fn test_sanitize_key_adjacent_to_any_control_sequence_is_redacted() {
    for seq in SEQUENCES {
        for input in [
            format!("x{seq}{}", asia()),
            format!("{}{seq}x", asia()),
            format!("pre {seq} {} post", asia()),
        ] {
            let out = clean(&input);
            assert!(!out.contains("ASIA"), "{seq:?} in {input:?} left {out:?}");
        }
    }
}

#[test]
fn test_sanitize_key_split_in_the_middle_by_any_control_sequence_is_redacted() {
    let key = asia();
    let (head, tail) = key.split_at(8);
    for seq in SEQUENCES {
        let out = clean(&format!("id={head}{seq}{tail} end"));
        assert!(
            !out.contains("ASIA") && !out.contains(tail),
            "{seq:?} left {out:?}"
        );
    }
}

#[test]
fn test_sanitize_prefixed_token_split_by_newline_is_redacted() {
    let out = clean("token npm_abcdefAB\nCDEF123 done");
    assert!(!out.contains("npm_"), "{out}");
    assert!(!out.contains("CDEF123"), "{out}");
}

#[test]
fn test_sanitize_osc_and_dcs_payloads_are_removed_not_just_the_escape() {
    let out = clean("a\x1b]0;evil-title\x07b\x1bPdcs-secret\x1b\\c");
    assert!(!out.contains("evil-title"), "{out}");
    assert!(!out.contains("dcs-secret"), "{out}");
    assert!(
        out.starts_with('a') && out.contains('b') && out.ends_with('c'),
        "{out}"
    );
}

#[test]
fn test_sanitize_literal_line_separators_are_mapped_away() {
    let out = clean(&format!("{}\u{2028}{} end", &asia()[..8], &asia()[8..]));
    assert!(!out.contains("ASIA"), "{out}");
    let out = clean("a\u{2029}b");
    assert!(!out.contains('\u{2029}'), "{out}");
}

#[test]
fn test_sanitize_newlines_outside_tokens_render_as_spaces() {
    assert_eq!(
        sanitize_upstream_text("line one\nline two\tend", 100),
        "line one line two end"
    );
}

#[test]
fn test_sanitize_text_without_controls_is_unchanged_and_truncated_by_chars() {
    assert_eq!(
        sanitize_upstream_text("plain text, 42 items.", 100),
        "plain text, 42 items."
    );
    assert_eq!(sanitize_upstream_text("abcdef", 3), "abc");
    assert!(clean(&format!("\n{}\n", "é".repeat(600))).chars().count() <= 500);
}
