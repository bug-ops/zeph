// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Regression inputs for #7145: `sanitize_upstream_text` keeps boundary-anchored shapes
//! (AWS STS key ids, whitespace-delimited tokens) redacted next to control characters while still
//! reassembling tokens split by a control or bypass codepoint.

use zeph_common::sanitize::strip_control_chars;
use zeph_common::secret_redact::sanitize_upstream_text;

fn asia() -> String {
    format!("{}{}", "ASIA", "ABCDEFGHIJKLMNOP")
}

fn clean(input: &str) -> String {
    let out = sanitize_upstream_text(input, 500);
    assert_eq!(
        out,
        strip_control_chars(&out),
        "strippable char left: {out:?}"
    );
    out
}

#[test]
fn sts_key_before_newline_is_redacted_and_next_line_kept() {
    let out = clean(&format!("id={}\nNext line", asia()));
    assert!(!out.contains(&asia()), "{out}");
    assert!(out.contains("Next line"), "{out}");
}

#[test]
fn sts_key_after_newline_is_redacted() {
    assert!(!clean(&format!("error\n{} end", asia())).contains(&asia()));
}

#[test]
fn sts_key_wrapped_in_ansi_color_is_redacted() {
    let out = clean(&format!("key \x1b[31m{}\x1b[0m", asia()));
    assert!(!out.contains(&asia()), "{out}");
}

#[test]
fn sts_key_next_to_bel_is_redacted_on_both_sides() {
    assert!(!clean(&format!("x\x07{}", asia())).contains(&asia()));
    assert!(!clean(&format!("{}\x07x", asia())).contains(&asia()));
}

#[test]
fn prefixed_secret_split_by_zero_width_space_is_redacted() {
    assert!(!clean("key\u{200B}sk-ant-xyz-shortsecret1").contains("sk-ant"));
}

#[test]
fn tab_and_bel_delimited_tokens_are_redacted() {
    assert!(!clean("error\tnpm_abcdefABCDEF123").contains("npm_"));
    assert!(!clean("x\x07hf_abcdEFGHijklMNOPqrst").contains("hf_"));
}

#[test]
fn bearer_token_after_tab_is_redacted() {
    let out = clean("Authorization: Bearer\tabcdefghijklmnopqrstuvwxyz012345");
    assert!(!out.contains("abcdefghijklmnopqrstuvwxyz012345"), "{out}");
}

#[test]
fn fixed_length_shape_split_by_bypass_codepoint_is_redacted() {
    let out = clean("ASIAABCDEFGH\u{200B}IJKLMNOP");
    assert!(!out.contains("ASIA") && !out.contains("IJKLMNOP"), "{out}");
}

#[test]
fn greedy_shape_split_by_bel_leaves_no_tail() {
    let jwt = format!(
        "{}{}",
        "eyJhbGciOiJIUzI1NiJ9", ".eyJzdWIiOiIxMjM0NTY3ODkwIn0."
    );
    let out = clean(&format!("{jwt}abc\x07defTAILCHARS"));
    assert!(!out.contains("eyJ") && !out.contains("TAILCHARS"), "{out}");
}

#[test]
fn whitespace_controls_become_a_space() {
    assert_eq!(clean("line one\nline two"), "line one line two");
}

#[test]
fn github_token_split_by_bare_escape_leaves_no_tail() {
    let out = clean("ghp_012345\x1b6789abcdefghijklmnopqrstuvwxyz");
    assert!(
        !out.contains("ghp_") && !out.contains("6789abcdef"),
        "{out}"
    );
}

#[test]
fn github_token_split_by_csi_sequence_leaves_no_tail() {
    let out = clean("ghp_012345\x1b[0m6789abcdefghijklmnopqrstuvwxyz");
    assert!(
        !out.contains("ghp_") && !out.contains("6789abcdef"),
        "{out}"
    );
}

#[test]
fn prefixed_secret_split_by_csi_sequence_leaves_no_tail() {
    let out = clean("key sk-proj-AbCdEf\x1b[31m0123456789GhIjKlMnOp end");
    assert!(
        !out.contains("sk-proj") && !out.contains("GhIjKlMnOp"),
        "{out}"
    );
}
