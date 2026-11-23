// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Regression tests for #7167: gluing words across a line break must never un-redact a shape
//! that ends at the break, and parameterless CSI must not leave its final letter behind.

use zeph_common::secret_redact::sanitize_upstream_text;

fn out(input: &str) -> String {
    sanitize_upstream_text(input, 10_000)
}

#[test]
fn test_glue_does_not_unredact_fixed_length_key_ending_at_break() {
    for input in [
        "ASIAIOSFODNN7EXAMPLE\nfoo ghp_0123456789abcdefghij\nklmnopqrstuvwxyzABCDEFGHIJKLMNOP",
        "id ASIAIOSFODNN7EXAMPLE\nnext sk-proj-abc\ndefghijklmnopqrstuvwxyz0123456789",
        "AKIAIOSFODNN7EXAMPLE\nx sk-ant-api03-abc\ndefghijklmnopqrstuvwxyz0123456789abcdef",
        "ASIAIOSFODNN7EXAMPLE\r\nnext line",
    ] {
        let o = out(input);
        assert!(!o.contains("IOSFODNN7EXAMPLE"), "{input:?} -> {o:?}");
    }
}

#[test]
fn test_glue_redacts_long_prefixed_key_split_by_newline() {
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q");
    let (head, tail) = key.split_at(20);
    let o = out(&format!("key {head}\n{tail} done"));
    assert!(!o.contains("a1B2c3D4"), "{o:?}");
    assert!(!o.contains(tail), "{o:?}");
}

#[test]
fn test_parameterless_csi_leaves_no_final_letter() {
    assert_eq!(
        out("line\u{1b}[Kdone \u{1b}[mok \u{1b}[Hhome"),
        "linedone ok home"
    );
}

#[test]
fn test_parameterless_csi_keeps_adjacent_key_redacted() {
    let o = out("\u{1b}[mAKIAIOSFODNN7EXAMPLE end");
    assert!(!o.contains("IOSFODNN7EXAMPLE"), "{o:?}");
}

#[test]
fn test_prose_across_lines_is_not_glued() {
    assert_eq!(out("alpha\nbeta\r\ngamma"), "alpha beta  gamma");
}

#[test]
fn test_escape_before_key_never_eats_its_first_character() {
    for input in [
        "x \u{1b}[sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF",
        "\u{1b}AKIAIOSFODNN7EXAMPLE",
        "\u{1b}AIzaSyA-abcdefghijklmnopqrstuvwxyz012345",
        "\u{1b}(sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
        "\u{9b}sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        let o = out(input);
        assert!(!o.contains("IOSFODNN7EXAMPLE"), "{input:?} -> {o:?}");
        assert!(
            !o.contains("abcdefghijklmnopqrstuvwxyz"),
            "{input:?} -> {o:?}"
        );
        assert!(!o.contains("SyA-abcdefghijkl"), "{input:?} -> {o:?}");
    }
}

#[test]
fn test_escape_before_pem_header_keeps_body_redacted() {
    let pem = "\u{1b}-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAabcdefghij\n-----END RSA PRIVATE KEY-----";
    let o = out(pem);
    assert!(!o.contains("MIIEowIBAAKCAQEA"), "{o:?}");
}

#[test]
fn test_key_wrapped_over_three_lines_is_redacted() {
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u1V2w3X4y");
    let (a, rest) = key.split_at(20);
    let (b, c) = rest.split_at(20);
    let o = out(&format!("key {a}\n{b}\n{c} end"));
    assert!(!o.contains(c) && !o.contains(&c[..8]), "{o:?}");
    assert!(o.ends_with("end"), "{o:?}");
}

#[test]
fn test_many_harmless_breaks_do_not_exhaust_the_probe_budget() {
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u1V2w3X4y");
    let (head, tail) = key.split_at(25);
    let lines = "at module.function(file.rs:12)\n".repeat(200);
    let o = out(&format!("{lines}key {head}\n{tail} end"));
    assert!(!o.contains(&tail[..8]), "{o:?}");
}

#[test]
fn test_escapes_never_eat_key_after_probe_budget_is_spent() {
    let filler = "\u{1b}(Babcdefghijklmnopqrstuv\n".repeat(400);
    for key in [
        "\u{1b}(sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF",
        "\u{1b}AKIAIOSFODNN7EXAMPLE",
    ] {
        let o = out(&format!("{filler}{key}"));
        assert!(!o.contains("IOSFODNN7EXAMPLE"), "{o:?}");
        assert!(!o.contains("api03-abcdefghijklmnop"), "{o:?}");
    }
}

#[test]
fn test_many_tput_sgr0_lines_do_not_affect_a_later_key() {
    let lines = "\u{1b}(B\u{1b}[m done\n".repeat(300);
    let o = out(&format!(
        "{lines}\u{1b}(sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF"
    ));
    assert!(!o.contains("api03-abcdefghijklmnop"), "{o:?}");
}

#[test]
fn test_escape_at_line_end_does_not_stop_wrapped_key_gluing() {
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u1V2w3X4y");
    let (head, tail) = key.split_at(25);
    for end in ["\u{1b}[0m", "\u{1b}[K"] {
        let o = out(&format!("key {head}{end}\n{tail} end"));
        assert!(!o.contains(&tail[..8]), "{end:?} -> {o:?}");
    }
}
