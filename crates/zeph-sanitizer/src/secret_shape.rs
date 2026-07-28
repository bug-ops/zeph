// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shape-based secret detection: redacts strings that *look like* a secret (known API-key
//! prefix, `Authorization: Bearer` header, standalone JWT) regardless of whether the value
//! was ever registered with a [`crate::secret_mask::SecretMaskRegistry`].
//!
//! [`SecretMaskRegistry`][crate::secret_mask::SecretMaskRegistry] only masks literal secret
//! *values* explicitly registered at runtime (actual vault-loaded secrets) — a string a
//! subagent invents, echoes, or fabricates in its own response text is invisible to it. This
//! module closes that gap by scanning for the same secret shapes `zeph-core::redact` already
//! scrubs from debug dumps and tool-execution output (issue #6571), compiling its own
//! `regex::Regex` instances from the canonical prefix/pattern constants in
//! [`zeph_common::secrets`] — the single source of truth both crates share.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use zeph_common::secrets::{
    AWS_SECRET_KEY_PATTERN, BEARER_TOKEN_PATTERN, JWT_PATTERN, PEM_PRIVATE_KEY_PATTERN,
    PEM_PRIVATE_KEY_UNTERMINATED_PATTERN, SECRET_PREFIXES,
};

// Matches any secret prefix followed by non-whitespace/quote/bracket characters. A single
// alternation pass covers every prefix in `SECRET_PREFIXES`; each prefix is regex-escaped
// since e.g. `ya29.` contains a literal dot that must not be treated as "match any character".
static SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = SECRET_PREFIXES
        .iter()
        .map(|p| regex::escape(p))
        .collect::<Vec<_>>()
        .join("|");
    let full = format!("(?:{pattern})[^\\s\"'`,;{{}}\\[\\]]*");
    Regex::new(&full).expect("secret shape regex is valid")
});

static BEARER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(BEARER_TOKEN_PATTERN).expect("bearer shape regex is valid"));

static JWT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(JWT_PATTERN).expect("jwt shape regex is valid"));

static PEM_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PEM_PRIVATE_KEY_PATTERN).expect("pem shape regex is valid"));

// Fallback for a PEM/SSH2 header with no matching footer (truncated input, adversarially
// unterminated, or a footer chunk dropped by a bounded ingress channel — see #6592 follow-up
// and `PEM_PRIVATE_KEY_UNTERMINATED_PATTERN`'s doc comment). Must run after `PEM_REGEX` so
// already-closed blocks are consumed first and only genuinely unterminated headers remain.
static PEM_UNTERMINATED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).expect("pem unterminated shape regex is valid")
});

static AWS_SECRET_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(AWS_SECRET_KEY_PATTERN).expect("aws secret shape regex is valid"));

/// Replace secret-shaped substrings (known API-key prefixes, `Authorization: Bearer` headers,
/// standalone JWTs, PEM private-key blocks, marker-anchored AWS secret access keys) with
/// redaction markers.
///
/// Unlike [`SecretMaskRegistry::mask`][crate::secret_mask::SecretMaskRegistry::mask], this
/// does not require the secret value to have been registered ahead of time — it flags
/// anything matching a known secret *shape*, so it also catches secrets a subagent fabricates
/// or echoes in generated text. Returns `Cow::Borrowed` when nothing matched (zero-allocation
/// fast path).
///
/// No existing pattern spanned a PEM key's multi-line body before this fix (see #6592) — the
/// `-----BEGIN` entry in [`SECRET_PREFIXES`] only ever matches a literal token on a single
/// line, regardless of scrub ordering. Two PEM passes run first, before the prefix pass: the
/// properly-closed-block pattern, then a footerless-header fallback (truncated/adversarial
/// input, or a footer chunk dropped by a bounded ingress channel). Running them first also
/// matters operationally — the prefix pass would otherwise consume just the `-----BEGIN`
/// token and prevent either PEM pattern from matching the header at all.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::secret_shape::scrub_secret_shapes;
///
/// let result = scrub_secret_shapes("here is a key: sk-test-abc123def456");
/// assert!(!result.contains("sk-test-abc123def456"));
/// assert!(result.contains("[REDACTED]"));
/// ```
#[must_use]
pub fn scrub_secret_shapes(text: &str) -> Cow<'_, str> {
    let after_pem: Cow<'_, str> = PEM_REGEX.replace_all(text, "[REDACTED_PEM_KEY]");

    let after_pem_fallback: Cow<'_, str> =
        match PEM_UNTERMINATED_REGEX.replace_all(after_pem.as_ref(), "[REDACTED_PEM_KEY]") {
            Cow::Borrowed(_) => after_pem,
            Cow::Owned(s) => Cow::Owned(s),
        };

    let has_prefix_match = SECRET_PREFIXES
        .iter()
        .any(|p| after_pem_fallback.contains(*p));
    let after_prefixes: Cow<'_, str> = if has_prefix_match {
        match SECRET_REGEX.replace_all(after_pem_fallback.as_ref(), "[REDACTED]") {
            Cow::Borrowed(_) => after_pem_fallback,
            Cow::Owned(s) => Cow::Owned(s),
        }
    } else {
        after_pem_fallback
    };

    let after_bearer: Cow<'_, str> =
        match BEARER_REGEX.replace_all(after_prefixes.as_ref(), "${1}[REDACTED]") {
            Cow::Borrowed(_) => after_prefixes,
            Cow::Owned(s) => Cow::Owned(s),
        };

    let after_jwt: Cow<'_, str> =
        match JWT_REGEX.replace_all(after_bearer.as_ref(), "[REDACTED_JWT]") {
            Cow::Borrowed(_) => after_bearer,
            Cow::Owned(s) => Cow::Owned(s),
        };

    match AWS_SECRET_REGEX.replace_all(after_jwt.as_ref(), "${1}[REDACTED]${2}") {
        Cow::Borrowed(_) => after_jwt,
        Cow::Owned(s) => Cow::Owned(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_generic_api_key_shape() {
        let text = "the key is sk-test-abc123def456, use it wisely";
        let result = scrub_secret_shapes(text);
        assert!(!result.contains("sk-test-abc123def456"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_bearer_token() {
        let result =
            scrub_secret_shapes("Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("eyJhbGciOiJSUzI1NiJ9"));
        assert!(result.contains("Authorization:"));
    }

    #[test]
    fn redacts_standalone_jwt() {
        let jwt = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIn0.SflKxwRJSMeKKF2";
        let text = format!("token value: {jwt} was found");
        let result = scrub_secret_shapes(&text);
        assert!(result.contains("[REDACTED_JWT]"));
        assert!(!result.contains("eyJhbGci"));
    }

    #[test]
    fn preserves_normal_text() {
        let text = "This is a normal response with no secrets";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn all_secret_prefixes_detected() {
        for prefix in SECRET_PREFIXES {
            let text = format!("token: {prefix}abc123");
            let result = scrub_secret_shapes(&text);
            assert!(result.contains("[REDACTED]"), "failed for prefix: {prefix}");
            assert!(!result.contains(*prefix), "prefix not redacted: {prefix}");
        }
    }

    #[test]
    fn redacts_full_pem_block() {
        let text = "here is my key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0B\nAQEFAASCAT8wggE7AgEAAkEA\n-----END RSA PRIVATE KEY-----\nthanks";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("MIIBVQIBADANBgkqhkiG9w0B"));
        assert!(result.contains("here is my key:"));
        assert!(result.contains("thanks"));
    }

    #[test]
    fn redacts_multiple_pem_blocks_independently() {
        let text = "-----BEGIN PRIVATE KEY-----\nfirstbody\n-----END PRIVATE KEY-----\nsome text in between\n-----BEGIN EC PRIVATE KEY-----\nsecondbody\n-----END EC PRIVATE KEY-----";
        let result = scrub_secret_shapes(text);
        assert_eq!(result.matches("[REDACTED_PEM_KEY]").count(), 2);
        assert!(!result.contains("firstbody"));
        assert!(!result.contains("secondbody"));
        assert!(result.contains("some text in between"));
    }

    #[test]
    fn no_false_positive_on_bare_begin_or_private_words() {
        let text = "Let's BEGIN the PRIVATE discussion about keys without any PEM markers";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn redacts_pem_block_with_mismatched_header_footer_labels() {
        // Documented over-match tradeoff: `regex` has no backreferences, so a footer whose
        // label doesn't match the header's label still gets redacted (over-matching is
        // acceptable; under-matching would leak key material).
        let text = "-----BEGIN RSA PRIVATE KEY-----\nbody-material\n-----END EC PRIVATE KEY-----";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("body-material"));
    }

    #[test]
    fn redacts_footerless_pem_header_via_fallback() {
        // C2: a PEM header with no matching footer at all (truncated input, adversarially
        // omitted, or a footer chunk dropped by a bounded channel) must still be redacted —
        // not just the header token, leaving the whole body exposed.
        let text = "here is my key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0B\nAQEFAASCAT8wggE7AgEAAkEA";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("MIIBVQIBADANBgkqhkiG9w0B"));
        assert!(!result.contains("-----BEGIN"));
        assert!(result.contains("here is my key:"));
    }

    #[test]
    fn adversarial_repeated_unterminated_pem_headers_are_bounded() {
        // M3 / anti-censorship guard: a subagent wrapping arbitrary content behind a forged
        // or genuinely unterminated header must not be able to make an unbounded amount of it
        // vanish — only up to PEM_BODY_CAP characters past each header are redacted; content
        // beyond that cap must remain visible rather than being silently swallowed.
        use zeph_common::secrets::PEM_BODY_CAP;

        let huge_body = "A".repeat(50_000);
        let text = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{huge_body}\n-----BEGIN EC PRIVATE KEY-----\n{huge_body}"
        );
        let result = scrub_secret_shapes(&text);
        assert_eq!(result.matches("[REDACTED_PEM_KEY]").count(), 2);
        // Each huge_body is far larger than PEM_BODY_CAP, so most of its 'A' characters must
        // still be present in the output — proving the fallback did not swallow it whole.
        let remaining_as = result.matches('A').count();
        assert!(
            remaining_as > huge_body.len() - PEM_BODY_CAP,
            "expected most of the oversized body to remain visible past the cap, got only \
             {remaining_as} 'A' chars remaining"
        );
    }

    #[test]
    fn redacts_marker_anchored_aws_secret_key() {
        let text = "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let result = scrub_secret_shapes(text);
        assert!(!result.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(result.contains("[REDACTED]"));
        assert!(result.contains("aws_secret_access_key="));
    }

    #[test]
    fn preserves_unanchored_high_entropy_string() {
        // Same 40-char base64-ish shape as a real AWS secret key, but with no marker nearby —
        // must not be flagged, or ordinary hashes/IDs would be false positives.
        let text = "build hash: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY was produced";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
    }
}
