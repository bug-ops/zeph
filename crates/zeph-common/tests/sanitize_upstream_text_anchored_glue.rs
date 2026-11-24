// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7184 and #7185: a secret split across lines is fully redacted for
//! every redaction shape, regardless of how many unanchored token-like lines precede it, and
//! context-anchored shapes (Bearer header, marker-anchored AWS secret) are glued.

use zeph_common::secret_redact::sanitize_upstream_text;

/// One realistic secret shape: text before the secret, the two halves around the line break,
/// text after, and the secret body whose 6-character windows must not survive.
struct Sample {
    shape: &'static str,
    before: &'static str,
    head: &'static str,
    tail: &'static str,
    after: &'static str,
    body: &'static str,
}

const SAMPLES: &[Sample] = &[
    Sample {
        shape: "sk-prefix",
        before: "key ",
        head: "sk-proj-AbCdEfGhIjKl",
        tail: "MnOpQrStUvWxYz0123456789",
        after: " end",
        body: "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
    },
    Sample {
        shape: "ghp-prefix",
        before: "token ",
        head: "ghp_0123456789abcdefghij",
        tail: "klmnopqrstuvwxyzABCDEFGHIJKLMNOP",
        after: " end",
        body: "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP",
    },
    Sample {
        shape: "github-fine-grained-pat",
        before: "pat ",
        head: "github_pat_11ABCDEFG0aBcDeFgHiJkL_mNoPqRsTuVwXyZ0123",
        tail: "456789aBcDeFgHiJkLmNoPqRsTuVwXyZ01234567",
        after: " end",
        body: "mNoPqRsTuVwXyZ0123456789aBcDeFgHiJkLmNoPqRsTuVwXyZ01234567",
    },
    Sample {
        shape: "github-app-token",
        before: "app ",
        head: "ghs_aB3dE6gH9jK2mN5pQ8sT",
        tail: "1vW4yZ7aB0cD3eF6gH9iJ2kL5mN8",
        after: " end",
        body: "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7aB0cD3eF6gH9iJ2kL5mN8",
    },
    Sample {
        shape: "telegram-bot-token-in-url",
        before: "GET https://api.telegram.org/",
        head: "bot123456789:AAEhBP0av28KJ",
        tail: "GwtnjnABCDEFGHIJKLMNOPqrstuvwxyz01",
        after: "/getMe failed",
        body: "EhBP0av28KJGwtnjnABCDEFGHIJKLMNOPqrstuvwxyz01",
    },
    Sample {
        shape: "aws-sts-access-key-id",
        before: "id ",
        head: "ASIAIOSFOD",
        tail: "NN7EXAMPLE",
        after: " end",
        body: "IOSFODNN7EXAMPLE",
    },
    Sample {
        shape: "bearer-header",
        before: "",
        head: "Authorization: Bearer abcdefghijklmnop",
        tail: "qrstuvwxyz0123456789",
        after: "",
        body: "abcdefghijklmnopqrstuvwxyz0123456789",
    },
    Sample {
        shape: "jwt",
        before: "jwt ",
        head: "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVP",
        tail: "mB92K27uhbUJU1p1rXyZ",
        after: " end",
        body: "dBjftJeZ4CVPmB92K27uhbUJU1p1rXyZ",
    },
    Sample {
        shape: "aws-secret-key-marker",
        before: "",
        head: "aws_secret_access_key = wJalrXUtnFEMIbK7MDENG",
        tail: "bPxRfiCYEXAMPLEKEY1234567",
        after: " end",
        body: "wJalrXUtnFEMIbK7MDENGbPxRfiCYEXAMPLEKEY1234567",
    },
];

fn windows(body: &str) -> impl Iterator<Item = &str> {
    (0..=body.len() - 6).map(move |i| &body[i..i + 6])
}

fn leaks(output: &str, body: &str) -> Option<String> {
    windows(body)
        .find(|w| output.contains(w))
        .map(str::to_owned)
}

fn preamble() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for i in 0..300 {
        writeln!(out, "{:032x}", i * 7919 + 123_456_789).expect("writing to a String");
    }
    out
}

fn failures(build: impl Fn(&Sample) -> String) -> Vec<String> {
    SAMPLES
        .iter()
        .filter_map(|s| {
            let out = sanitize_upstream_text(&build(s), 1_000_000);
            leaks(&out, s.body).map(|w| format!("{}: leaked {w:?} in {out:?}", s.shape))
        })
        .collect()
}

#[test]
fn test_sanitize_upstream_text_split_secret_redacted_for_every_shape() {
    let bad = failures(|s| format!("{}{}\n{}{}", s.before, s.head, s.tail, s.after));
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn test_sanitize_upstream_text_split_secret_redacted_after_300_unanchored_lines() {
    let pre = preamble();
    let bad = failures(|s| format!("{pre}{}{}\n{}{}", s.before, s.head, s.tail, s.after));
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn test_sanitize_upstream_text_unsplit_secret_redacted_as_before() {
    let bad = failures(|s| format!("{}{}{}{}", s.before, s.head, s.tail, s.after));
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn test_sanitize_upstream_text_issue_7184_repro_redacts_key_tail() {
    let text = format!(
        "{}key sk-proj-AbCdEfGhIjKl\nMnOpQrStUvWxYz0123456789 end",
        preamble()
    );
    let out = sanitize_upstream_text(&text, 1_000_000);
    assert!(!out.contains("MnOpQrStUvWxYz0123456789"), "{out:?}");
}

#[test]
fn test_sanitize_upstream_text_issue_7185_bearer_continuation_is_redacted() {
    let out = sanitize_upstream_text(
        "Authorization: Bearer abcdefghijklmnop\nqrstuvwxyz0123456789",
        1000,
    );
    assert!(!out.contains("qrstuvwxyz0123456789"), "{out:?}");
}

#[test]
fn test_sanitize_upstream_text_unanchored_lines_are_not_consumed_by_glue() {
    let pre = preamble();
    let out = sanitize_upstream_text(&format!("{pre}tail"), 1_000_000);
    assert!(
        out.starts_with(&pre.lines().next().unwrap()[..32]),
        "{out:?}"
    );
    assert!(out.ends_with("tail"), "{out:?}");
}
