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
use zeph_common::secrets::{BEARER_TOKEN_PATTERN, JWT_PATTERN, SECRET_PREFIXES};

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

/// Replace secret-shaped substrings (known API-key prefixes, `Authorization: Bearer` headers,
/// standalone JWTs) with redaction markers.
///
/// Unlike [`SecretMaskRegistry::mask`][crate::secret_mask::SecretMaskRegistry::mask], this
/// does not require the secret value to have been registered ahead of time — it flags
/// anything matching a known secret *shape*, so it also catches secrets a subagent fabricates
/// or echoes in generated text. Returns `Cow::Borrowed` when nothing matched (zero-allocation
/// fast path).
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
    let has_prefix_match = SECRET_PREFIXES.iter().any(|p| text.contains(*p));
    let after_prefixes: Cow<'_, str> = if has_prefix_match {
        SECRET_REGEX.replace_all(text, "[REDACTED]")
    } else {
        Cow::Borrowed(text)
    };

    let after_bearer: Cow<'_, str> =
        match BEARER_REGEX.replace_all(after_prefixes.as_ref(), "${1}[REDACTED]") {
            Cow::Borrowed(_) => after_prefixes,
            Cow::Owned(s) => Cow::Owned(s),
        };

    match JWT_REGEX.replace_all(after_bearer.as_ref(), "[REDACTED_JWT]") {
        Cow::Borrowed(_) => after_bearer,
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
}
