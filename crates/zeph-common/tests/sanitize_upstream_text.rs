// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests (tests-first) for #7145: `secret_redact::sanitize_upstream_text` scrubs secret
//! shapes (including tokens split by control or bypass codepoints), strips control and bypass
//! codepoints, and caps the result.

use zeph_common::sanitize::strip_control_chars;
use zeph_common::secret_redact::sanitize_upstream_text;

const TOKEN: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
const SPLITTERS: [char; 4] = ['\u{200B}', '\u{202E}', '\u{1b}', '\u{7}'];

#[test]
fn sanitize_upstream_text_redacts_plain_token() {
    let out = sanitize_upstream_text(&format!("bad creds {TOKEN} end"), 500);
    assert!(!out.contains(TOKEN), "{out}");
    assert!(!out.contains("ghp_"), "{out}");
}

#[test]
fn sanitize_upstream_text_redacts_token_split_by_control_or_bypass_codepoint() {
    for splitter in SPLITTERS {
        let (head, tail) = TOKEN.split_at(10);
        let out = sanitize_upstream_text(&format!("x {head}{splitter}{tail} y"), 500);
        assert!(
            !out.contains("ghp_"),
            "split by U+{:04X}: {out}",
            splitter as u32
        );
        assert!(
            !out.contains(tail),
            "split by U+{:04X}: {out}",
            splitter as u32
        );
    }
}

#[test]
fn sanitize_upstream_text_output_has_no_strippable_chars() {
    let input = "a\u{200B}b\u{202E}c\u{2060}d\u{FEFF}e\u{1b}[31mf\u{7}g\nh";
    let out = sanitize_upstream_text(input, 500);
    assert_eq!(out, strip_control_chars(&out));
}

#[test]
fn sanitize_upstream_text_caps_at_max_chars() {
    let out = sanitize_upstream_text(&"é".repeat(2000), 500);
    assert_eq!(out.chars().count(), 500);
}

#[test]
fn sanitize_upstream_text_cap_never_cuts_secret_before_scrubbing() {
    let input = format!("{}{TOKEN}", "x".repeat(495));
    let out = sanitize_upstream_text(&input, 500);
    assert!(out.chars().count() <= 500);
    assert!(!out.contains("ghp_"), "{out}");
}

#[test]
fn sanitize_upstream_text_leaves_clean_text_unchanged() {
    assert_eq!(
        sanitize_upstream_text("rate limit exceeded", 500),
        "rate limit exceeded"
    );
}
