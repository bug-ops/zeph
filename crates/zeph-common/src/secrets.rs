// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Canonical secret-token and path prefixes shared by redaction layers across crates.
//!
//! `zeph-core::redact` and `zeph-memory::store::compression_guidelines` both scrub
//! secrets and filesystem paths before persisting or displaying untrusted text. Each
//! crate previously carried its own hand-rolled copy of these lists, and the copies had
//! already begun to drift from each other (see #5917). This module is the single source
//! of truth for the raw prefixes/patterns; consumers compile their own `regex::Regex`
//! instances from these constants — `zeph-common` does not depend on `regex` outside of
//! tests, matching the pattern established by [`crate::patterns`].

/// Prefixes of API keys, tokens, and other secret material recognized across Zeph.
///
/// Each entry is a literal prefix, not regex-escaped. Consumers building a regex
/// alternation from this list must escape entries themselves (e.g. via `regex::escape`),
/// since `ya29.` contains a literal `.` that must not be treated as "match any character".
pub const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "AKIA",
    "ghp_",
    "gho_",
    "-----BEGIN",
    "xoxb-",
    "xoxp-",
    "AIza",
    "ya29.",
    "glpat-",
    "hf_",
    "npm_",
    "dckr_pat_",
];

/// Absolute filesystem path prefixes redacted before persisting or displaying untrusted
/// text, to avoid leaking local usernames or directory layout.
pub const PATH_PREFIXES: &[&str] = &["/home/", "/Users/", "/root/", "/tmp/", "/var/"];

/// Regex pattern matching `Authorization: Bearer <token>` headers.
///
/// Capture group 1 covers the header name up to and including the token's leading
/// whitespace, so replacing with `"${1}[REDACTED]"` preserves the header name while
/// redacting only the token value.
pub const BEARER_TOKEN_PATTERN: &str = r"(?i)(Authorization:\s*Bearer\s+)\S+";

/// Regex pattern matching standalone JWTs (three Base64url-encoded segments separated by
/// dots).
///
/// The final segment uses `*` (not `+`) so it also matches `alg=none` JWTs, which carry an
/// empty signature segment.
pub const JWT_PATTERN: &str = r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*";

/// Regex pattern matching a full, properly-closed PEM/SSH2 private-key block, from the
/// `-----BEGIN ... PRIVATE KEY-----`-style header through the matching footer, inclusive of
/// the body between them (see #6592).
///
/// The root cause reported in #6592 is that no existing detector spanned a PEM key's
/// multi-line body at all: the `-----BEGIN` entry in [`SECRET_PREFIXES`] only ever matches a
/// literal prefix on a single line by construction, so it was never capable of covering the
/// base64 body regardless of scrub ordering. Ordering still matters operationally, though:
/// running the prefix-based scrub before this pattern would let it consume just the
/// `-----BEGIN` token, preventing this pattern from ever matching the header again — which is
/// why PEM scrubbing must run first in `zeph_sanitizer::secret_shape::scrub_secret_shapes`'s
/// pipeline.
///
/// The `(?s)` flag lets `.` match newlines so the pattern spans the full multi-line body. The
/// body is matched non-greedily and bounded to at most [`PEM_BODY_CAP`] characters
/// (`.{0,8192}?`) — generous headroom over a real key (RSA-4096 PEM is ~3.2 KB; a bound large
/// enough to also cover RSA-4096 was originally chosen at 65,536, but `regex` rejected that
/// pattern with `CompiledTooBig` under its default 10 MB compiled-program size limit, so
/// [`PEM_BODY_CAP`] is deliberately the largest power-of-two-ish bound confirmed to compile
/// under that limit) — so that (a) consecutive PEM blocks in the same text are each redacted
/// individually rather than swallowed into one match, and (b) a subagent cannot wrap arbitrary
/// transcript content between forged `-----BEGIN`/`-----END` markers to make an unbounded
/// amount of it vanish from a display surface. `regex` does not support backreferences, so the
/// footer's label is not required to match the header's label — over-matching a mismatched
/// pair is an acceptable tradeoff for a redaction scanner, since under-matching leaks key
/// material. A header with no matching footer at all (truncated input, or a footer chunk
/// dropped in a bounded channel) does not match this pattern — see
/// [`PEM_PRIVATE_KEY_UNTERMINATED_PATTERN`], which must be applied afterward to still redact
/// that case.
///
/// Keep the header/footer marker alternation (`-----BEGIN ... PRIVATE KEY(?: BLOCK)?-----` /
/// the RFC 4716 `---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----` alternative, and the matching
/// `END` forms) in sync with [`PEM_PRIVATE_KEY_UNTERMINATED_PATTERN`]'s header alternation —
/// they must recognize exactly the same set of header markers.
pub const PEM_PRIVATE_KEY_PATTERN: &str = r"(?s)(?:-----BEGIN (?:[A-Z]+ )?PRIVATE KEY(?: BLOCK)?-----|---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----).{0,8192}?(?:-----END (?:[A-Z]+ )?PRIVATE KEY(?: BLOCK)?-----|---- END SSH2 ENCRYPTED PRIVATE KEY ----)";

/// Cap, in characters, on the PEM/SSH2 body matched by [`PEM_PRIVATE_KEY_PATTERN`] and
/// [`PEM_PRIVATE_KEY_UNTERMINATED_PATTERN`]. Kept as a named constant so the value referenced
/// in both patterns' doc comments (and in `zeph_subagent::forward`'s streaming holdback cap,
/// which must buffer at least this many bytes past an unclosed header before force-flushing)
/// stays traceable to one definition, even though the patterns themselves are plain `&str`
/// literals (regex patterns can't be built from a `const usize` via string formatting at
/// const-eval time without an extra dependency, so the literal `8192` is duplicated in both
/// pattern strings — keep it in sync with this constant if it ever changes).
///
/// Known accepted tradeoff (#6592 follow-up, "M5"): a **properly terminated** block whose body
/// exceeds this cap cannot be matched by [`PEM_PRIVATE_KEY_PATTERN`] (the footer lies past the
/// non-greedy bound), so [`PEM_PRIVATE_KEY_UNTERMINATED_PATTERN`]'s fallback redacts only the
/// first `PEM_BODY_CAP` characters of the body — the remaining tail, plus the now-orphaned
/// `-----END...` footer text, is left unredacted. Real keys are comfortably under this cap
/// (RSA-4096 PEM ≈ 3.2 KB), so the realistic risk is low, but this is a direct, deliberate
/// tension with the cap's other purpose — bounding how much a forged/unterminated header can
/// hide (see the primary pattern's doc comment, point (b), and the
/// `adversarial_repeated_unterminated_pem_headers_are_bounded` test in `zeph-sanitizer`, which
/// asserts over-cap content *must* stay visible for exactly the opposite reason). One bound
/// cannot simultaneously guarantee "every terminated block, however large, is fully redacted"
/// and "an unterminated/forged block can only ever hide a bounded amount of surrounding
/// content" — this module chooses the anti-censorship guarantee and accepts the oversized-
/// terminated-block gap as the cost, on the grounds that a real key exceeding this cap is a
/// vanishingly rare shape to encounter versus a subagent hiding a large amount of legitimate
/// transcript behind a forged pair of markers.
pub const PEM_BODY_CAP: usize = 8192;

/// Fallback regex matching a PEM/SSH2 private-key header with no matching footer found within
/// [`PEM_BODY_CAP`] characters — a header that is truncated, adversarially left unterminated,
/// or whose footer chunk was dropped by a bounded ingress channel (see #6592 follow-up).
///
/// Must be applied *after* [`PEM_PRIVATE_KEY_PATTERN`]'s replace pass, so that every properly
/// closed block has already been consumed and only genuinely unterminated headers remain —
/// otherwise this pattern's greedy, footer-agnostic match would swallow a following
/// already-valid block's header too.
///
/// The body is constrained to characters that can actually occur in a PEM body — base64
/// alphabet plus whitespace (`[A-Za-z0-9+/=\s]{0,8192}`) — rather than "any character"
/// (`.{0,8192}`). An earlier version of this pattern used `.{0,8192}`, which meant *any* text
/// following an unterminated header (e.g. `"...PRIVATE KEY----- in file /etc/ssl/key.pem and
/// the deploy failed"`) was swallowed wholesale up to the cap, silently destroying up to 8 KB
/// of unrelated legitimate content whenever a subagent merely *mentioned* a PEM header without
/// including a body (see #6592 follow-up, "S3" — this was a real over-redaction regression,
/// not hypothetical). Constraining the body to PEM-plausible characters makes the match stop
/// at the first character that cannot appear in base64 (e.g. the first `.`, `,`, or other
/// prose punctuation), which keeps genuinely truncated-key coverage while collapsing false-
/// positive over-redaction on ordinary prose to near zero — plain English text almost always
/// contains such a character within a few words.
///
/// Known narrow accepted gap (#6592 follow-up, "M7"): a *footerless* legacy encrypted PEM
/// (`Proc-Type: 4,ENCRYPTED`) or PGP (`Version: GnuPG v2`) armor's header/comment line contains
/// `:`, `,`, and other characters outside this class, so the match stops at that line and the
/// base64 body after it is left unredacted in this specific truncated-input case. Terminated
/// blocks of these same armor types are unaffected (they're covered by
/// [`PEM_PRIVATE_KEY_PATTERN`], which has no character-class restriction on the body). Not
/// fixed here — widening the class to cover armor-header-line punctuation would reopen most of
/// the S3 over-redaction blast radius this pattern exists to close.
pub const PEM_PRIVATE_KEY_UNTERMINATED_PATTERN: &str = r"(?:-----BEGIN (?:[A-Z]+ )?PRIVATE KEY(?: BLOCK)?-----|---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----)[A-Za-z0-9+/=\s]{0,8192}";

/// Regex pattern matching a raw AWS secret access key or session token immediately preceded
/// by a recognizable marker and its assignment separator (see #6592).
///
/// Unlike the `AKIA`-prefixed access key ID, an AWS secret access key (and session token) has
/// no distinguishing prefix — it is just a base64-ish string, indistinguishable by shape alone
/// from an ordinary hash or identifier. Flagging *every* base64-ish run of that length would
/// produce excessive false positives, so this pattern only fires when the value is directly
/// anchored to a recognizable marker name.
///
/// The marker alternation covers `aws_secret_access_key` / `aws_secret_key` /
/// `secret_access_key` / `aws_session_token` / `session_token`, each tolerant of `_`, `-`,
/// `.`, or a space as the inter-word separator (or none at all) — so the same alternation
/// also matches the camelCase JSON key names AWS's own tooling emits verbatim
/// (`SecretAccessKey`, `SessionToken`, e.g. from `aws configure export-credentials` or an STS
/// `AssumeRole` response), not just underscore-joined config-file names, following the
/// broader separator conventions gitleaks/trufflehog use for this rule class. An optional
/// quote is tolerated both before the separator (closing a quoted JSON key) and around the
/// value.
///
/// The value itself matches 40 or more base64-alphabet characters plus up to two `=` padding
/// characters (`{40,}={0,2}`, not a fixed `{40}`) so a longer-than-standard value is redacted
/// in full rather than leaking everything past the 40th character.
///
/// Capture group 1 covers the marker, separator, and optional opening quote; capture group 2
/// covers an optional closing quote. Replacing with `"${1}[REDACTED]${2}"` preserves the
/// marker and quoting while redacting only the secret value.
pub const AWS_SECRET_KEY_PATTERN: &str = r#"(?i)((?:aws[_\-. ]?secret[_\-. ]?access[_\-. ]?key|aws[_\-. ]?secret[_\-. ]?key|secret[_\-. ]?access[_\-. ]?key|aws[_\-. ]?session[_\-. ]?token|session[_\-. ]?token)['"]?\s*[:=]\s*['"]?)[A-Za-z0-9+/]{40,}={0,2}(['"]?)"#;

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    #[test]
    fn bearer_pattern_compiles_and_matches() {
        let re = Regex::new(BEARER_TOKEN_PATTERN).unwrap();
        assert!(re.is_match("Authorization: Bearer abc.def.ghi"));
    }

    #[test]
    fn jwt_pattern_compiles_and_matches() {
        let re = Regex::new(JWT_PATTERN).unwrap();
        assert!(re.is_match("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.sig"));
    }

    #[test]
    fn jwt_pattern_matches_alg_none_empty_signature() {
        let re = Regex::new(JWT_PATTERN).unwrap();
        assert!(re.is_match("eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyIn0."));
    }

    #[test]
    fn secret_prefixes_build_valid_escaped_regex_alternation() {
        let pattern = SECRET_PREFIXES
            .iter()
            .map(|p| regex::escape(p))
            .collect::<Vec<_>>()
            .join("|");
        let full = format!("(?:{pattern})[^\\s]*");
        let re = Regex::new(&full).expect("alternation built from SECRET_PREFIXES must compile");
        assert!(re.is_match("sk-abc123"));
        assert!(re.is_match("ya29.a0AfH6"));
    }

    #[test]
    fn path_prefixes_build_valid_regex_alternation() {
        let pattern = PATH_PREFIXES.join("|");
        let full = format!("(?:{pattern})[^\\s]*");
        let re = Regex::new(&full).expect("alternation built from PATH_PREFIXES must compile");
        assert!(re.is_match("/home/user/file"));
    }

    #[test]
    fn pem_pattern_compiles_and_matches_plain_private_key() {
        let re = Regex::new(PEM_PRIVATE_KEY_PATTERN).unwrap();
        let pem =
            "-----BEGIN PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0B\n-----END PRIVATE KEY-----";
        assert!(re.is_match(pem));
    }

    #[test]
    fn pem_pattern_matches_common_label_variants() {
        let re = Regex::new(PEM_PRIVATE_KEY_PATTERN).unwrap();
        for label in ["RSA", "EC", "DSA", "OPENSSH", "ENCRYPTED"] {
            let pem = format!(
                "-----BEGIN {label} PRIVATE KEY-----\nbase64body\n-----END {label} PRIVATE KEY-----"
            );
            assert!(re.is_match(&pem), "failed for label: {label}");
        }
    }

    #[test]
    fn pem_pattern_matches_pgp_block_and_ssh2_rfc4716_variants() {
        let re = Regex::new(PEM_PRIVATE_KEY_PATTERN).unwrap();
        let pgp = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nbase64body\n-----END PGP PRIVATE KEY BLOCK-----";
        assert!(re.is_match(pgp), "failed for PGP PRIVATE KEY BLOCK");
        let ssh2 = "---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----\nbase64body\n---- END SSH2 ENCRYPTED PRIVATE KEY ----";
        assert!(re.is_match(ssh2), "failed for RFC 4716 SSH2 marker");
    }

    #[test]
    fn pem_pattern_matches_mismatched_header_footer_labels() {
        // Documented tradeoff: `regex` has no backreferences, so a footer whose label does
        // not match the header's label still matches (over-matching, not under-matching).
        let re = Regex::new(PEM_PRIVATE_KEY_PATTERN).unwrap();
        let mismatched =
            "-----BEGIN RSA PRIVATE KEY-----\nbase64body\n-----END EC PRIVATE KEY-----";
        assert!(
            re.is_match(mismatched),
            "mismatched header/footer labels must still match (accepted over-match tradeoff)"
        );
    }

    #[test]
    fn pem_pattern_does_not_match_partial_markers() {
        let re = Regex::new(PEM_PRIVATE_KEY_PATTERN).unwrap();
        assert!(!re.is_match("this text mentions BEGIN and PRIVATE but no PEM markers"));
        assert!(!re.is_match("-----BEGIN PRIVATE KEY----- with no matching end marker"));
    }

    #[test]
    fn pem_unterminated_pattern_matches_footerless_header() {
        let re = Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).unwrap();
        assert!(re.is_match("-----BEGIN RSA PRIVATE KEY-----\nbase64body with no end marker"));
        assert!(!re.is_match("this text mentions BEGIN and PRIVATE but no PEM markers"));
    }

    #[test]
    fn pem_unterminated_pattern_body_is_bounded() {
        // Adversarial input: a header that never closes, with a body far larger than the
        // pattern's cap. The match must not extend past the bound (M3 / censorship-vector
        // guard) — asserted by checking the matched span length rather than just "matches".
        let re = Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).unwrap();
        let huge_body = "A".repeat(200_000);
        let text = format!("-----BEGIN RSA PRIVATE KEY-----\n{huge_body}");
        let m = re
            .find(&text)
            .expect("unterminated header must still match");
        let header_len = "-----BEGIN RSA PRIVATE KEY-----".len();
        assert!(
            m.len() <= header_len + PEM_BODY_CAP,
            "match length {} exceeds header + {PEM_BODY_CAP}-char body cap",
            m.len()
        );
    }

    #[test]
    fn pem_unterminated_pattern_handles_repeated_begin_with_no_end() {
        // Adversarial input: multiple unterminated headers in sequence, none ever closed.
        let re = Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).unwrap();
        let text = "-----BEGIN RSA PRIVATE KEY-----\nfirst\n-----BEGIN EC PRIVATE KEY-----\nsecond";
        assert!(re.is_match(text));
    }

    #[test]
    fn pem_unterminated_pattern_stops_at_first_non_pem_character() {
        // S3 regression guard: the fallback body is constrained to PEM-plausible characters
        // (base64 alphabet + whitespace), so it must stop matching at the first character
        // that cannot occur in a PEM body (e.g. a period) rather than swallowing an entire
        // sentence of ordinary prose up to the 8192-char cap.
        let re = Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).unwrap();
        let text =
            "Found -----BEGIN RSA PRIVATE KEY----- in file /etc/ssl/key.pem and the deploy failed";
        let m = re.find(text).expect("header must still match");
        assert!(
            !m.as_str().contains("and the deploy failed"),
            "match must stop well before swallowing unrelated trailing prose: {:?}",
            m.as_str()
        );
        assert!(
            text[m.end()..].contains("and the deploy failed"),
            "trailing prose must remain outside the match, available for the caller to keep: {:?}",
            &text[m.end()..]
        );
    }

    #[test]
    fn aws_secret_pattern_compiles_and_matches_marker_anchored_value() {
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        assert!(re.is_match("aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    }

    #[test]
    fn aws_secret_pattern_matches_camelcase_json_key_form() {
        // S2: the canonical STS/`aws configure export-credentials`/SDK JSON key form.
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        assert!(re.is_match(r#""SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY""#));
        assert!(re.is_match(r#""SessionToken": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY""#));
    }

    #[test]
    fn aws_secret_pattern_matches_alternate_separator_forms() {
        // S2: gitleaks/trufflehog-style separator flexibility beyond a literal underscore.
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        assert!(re.is_match("aws-secret-key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(re.is_match("aws.secret.key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(re.is_match("aws secret key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(re.is_match("aws_session_token=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    }

    #[test]
    fn aws_secret_pattern_redacts_full_longer_than_standard_value() {
        // M1: `{40,}` must not truncate the match at 40 chars for a longer value.
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        let long_value = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEYEXTRA1234"; // 50 chars
        let text = format!("aws_secret_access_key={long_value}");
        let m = re.find(&text).expect("must match");
        assert!(
            m.as_str().ends_with(long_value),
            "match must cover the full value, not just the first 40 chars: {}",
            m.as_str()
        );
    }

    #[test]
    fn aws_secret_pattern_ignores_unanchored_high_entropy_string() {
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        // Same shape (40-char base64-ish run) but no marker precedes it.
        assert!(!re.is_match("commit sha or hash: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    }

    #[test]
    fn aws_secret_pattern_ignores_marker_like_identifier_suffix() {
        // Explicitly confirmed non-finding: the marker must not fire on an identifier that
        // merely starts with a marker name followed by more identifier characters.
        let re = Regex::new(AWS_SECRET_KEY_PATTERN).unwrap();
        assert!(!re.is_match("let aws_secret_access_key_length = 40"));
    }
}
