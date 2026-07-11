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
}
