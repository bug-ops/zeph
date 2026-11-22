// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7145: `sanitize_upstream_text` keeps benign text that surrounds
//! control or bypass codepoints, removing only the strippable characters.

use zeph_common::secret_redact::sanitize_upstream_text;

#[test]
fn sanitize_upstream_text_keeps_plain_words_around_ansi_sequence() {
    let out = sanitize_upstream_text("rate\u{1b}[31m limit", 500);
    assert!(out.contains("rate"), "{out}");
    assert!(out.contains("limit"), "{out}");
    assert!(!out.contains('\u{1b}'), "{out}");
}

#[test]
fn sanitize_upstream_text_keeps_plain_text_around_lone_control_char() {
    assert_eq!(sanitize_upstream_text("ab\u{7}cd", 500), "abcd");
}

#[test]
fn sanitize_upstream_text_keeps_plain_text_around_bypass_codepoint() {
    assert_eq!(sanitize_upstream_text("ab\u{200B}cd", 500), "abcd");
}
