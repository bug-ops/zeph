// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use zeph_common::secrets::{BEARER_TOKEN_PATTERN, JWT_PATTERN, PATH_PREFIXES, SECRET_PREFIXES};

/// Apply URL-credential stripping, secret redaction (including Bearer headers and JWTs),
/// and path sanitization in a single pass.
///
/// Returns `Cow::Borrowed` when no changes are needed (zero-allocation fast path).
#[must_use]
pub fn scrub_content(text: &str) -> Cow<'_, str> {
    // Strip URL-embedded credentials first (https://user:pass@host → https://[REDACTED]@host).
    let after_url: Cow<'_, str> = URL_CREDS_REGEX.replace_all(text, "${scheme}[REDACTED]@");
    let after_secrets: Cow<'_, str> = match redact_secrets(after_url.as_ref()) {
        Cow::Borrowed(_) => after_url,
        Cow::Owned(s) => Cow::Owned(s),
    };
    match sanitize_paths(after_secrets.as_ref()) {
        Cow::Borrowed(_) => after_secrets,
        Cow::Owned(s) => Cow::Owned(s),
    }
}

// Matches any secret prefix followed by non-whitespace characters.
// Using alternation so a single pass covers all prefixes. Prefixes come from
// zeph_common::secrets::SECRET_PREFIXES (the canonical, unescaped list — see #5917)
// and are regex-escaped here since e.g. `ya29.` contains a literal dot.
static SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = SECRET_PREFIXES
        .iter()
        .map(|p| regex::escape(p))
        .collect::<Vec<_>>()
        .join("|");
    let full = format!("(?:{pattern})[^\\s\"'`,;{{}}\\[\\]]*");
    Regex::new(&full).expect("secret redaction regex is valid")
});

static PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let alt = PATH_PREFIXES.join("|");
    let full = format!(r#"(?:{alt})[^\s"'`,;{{}}\[\]]*"#);
    Regex::new(&full).expect("path redaction regex is valid")
});

// Matches basic-auth credentials embedded in URLs: https://user:pass@host
static URL_CREDS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<scheme>[a-z][a-z0-9+\-.]*://)(?P<creds>[^@/\s]+:[^@/\s]+@)")
        .expect("url credential redaction regex is valid")
});

// Matches `Authorization: Bearer <token>` headers, keeping the header name via capture group 1.
static BEARER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(BEARER_TOKEN_PATTERN).expect("bearer redaction regex is valid"));

// Matches standalone JWTs (three Base64url-encoded segments separated by dots).
static JWT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(JWT_PATTERN).expect("jwt redaction regex is valid"));

/// Replace tokens containing known secret patterns with `[REDACTED]`.
///
/// Detects secrets embedded in URLs, JSON values, and quoted strings (via the
/// [`zeph_common::secrets::SECRET_PREFIXES`] prefix list), `Authorization: Bearer` headers,
/// and standalone JWTs. Returns `Cow::Borrowed` when nothing was redacted (zero-allocation
/// fast path).
#[must_use]
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    // Fast path: check for any prefix substring before running the (more expensive)
    // alternation regex. Bearer/JWT regexes below are cheap enough to run unconditionally —
    // `Regex::replace_all` itself returns `Cow::Borrowed` when there is no match.
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

/// Replace absolute filesystem paths with `[PATH]` to prevent information disclosure.
#[must_use]
pub fn sanitize_paths(text: &str) -> Cow<'_, str> {
    if !PATH_PREFIXES.iter().any(|p| text.contains(*p)) {
        return Cow::Borrowed(text);
    }

    let result = PATH_REGEX.replace_all(text, "[PATH]");
    match result {
        Cow::Borrowed(_) => Cow::Borrowed(text),
        Cow::Owned(s) => Cow::Owned(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn redacts_openai_key() {
        let text = "Use key sk-abc123def456 for API calls";
        let result = redact_secrets(text);
        assert_eq!(result, "Use key [REDACTED] for API calls");
    }

    #[test]
    fn redacts_stripe_live_key() {
        let text = "Stripe key: sk_live_abcdef123456";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk_live_"));
    }

    #[test]
    fn redacts_stripe_test_key() {
        let text = "Test key sk_test_abc123";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_aws_key() {
        let text = "AWS access key: AKIAIOSFODNN7EXAMPLE";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("AKIA"));
    }

    #[test]
    fn redacts_github_pat() {
        let text = "Token: ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_"));
    }

    #[test]
    fn redacts_github_oauth() {
        let text = "OAuth: gho_xxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_private_key_header() {
        let text = "Found -----BEGIN RSA PRIVATE KEY----- in file";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("-----BEGIN"));
    }

    #[test]
    fn redacts_slack_tokens() {
        let text = "Bot token xoxb-123-456 and user xoxp-789";
        let result = redact_secrets(text);
        assert_eq!(result, "Bot token [REDACTED] and user [REDACTED]");
    }

    #[test]
    fn preserves_normal_text() {
        let text = "This is a normal response with no secrets";
        let result = redact_secrets(text);
        assert_eq!(result, text);
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn handles_empty_string() {
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn multiple_secrets_redacted() {
        let text = "Keys: sk-abc123 AKIAIOSFODNN7 ghp_xxxxx";
        let result = redact_secrets(text);
        assert_eq!(result, "Keys: [REDACTED] [REDACTED] [REDACTED]");
    }

    #[test]
    fn preserves_multiline_whitespace() {
        let text = "Line one\n  indented line\n\ttabbed line\nsk-secret here";
        let result = redact_secrets(text);
        assert_eq!(
            result,
            "Line one\n  indented line\n\ttabbed line\n[REDACTED] here"
        );
    }

    #[test]
    fn preserves_code_block_formatting() {
        let text = "```rust\nfn main() {\n    let key = \"sk-abc123\";\n    println!(\"{}\", key);\n}\n```";
        let result = redact_secrets(text);
        assert!(result.contains("```rust\nfn"));
        assert!(result.contains("    let"));
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123"));
    }

    #[test]
    fn preserves_multiple_spaces() {
        let text = "word1   word2     word3";
        let result = redact_secrets(text);
        assert_eq!(result, text);
    }

    #[test]
    fn no_allocation_without_secrets() {
        let text = "safe text without any secrets";
        let result = redact_secrets(text);
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn all_secret_prefixes_tested() {
        for prefix in SECRET_PREFIXES {
            let text = format!("token: {prefix}abc123");
            let result = redact_secrets(&text);
            assert!(result.contains("[REDACTED]"), "Failed for prefix: {prefix}");
            assert!(!result.contains(*prefix), "Prefix not redacted: {prefix}");
        }
    }

    #[test]
    fn redacts_google_api_key() {
        let text = "Google key: AIzaSyA1234567890abcdefghijklmnop";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("AIza"));
    }

    #[test]
    fn redacts_google_oauth_token() {
        let text = "OAuth token ya29.a0AfH6SMBx1234567890";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ya29."));
    }

    #[test]
    fn redacts_gitlab_pat() {
        let text = "GitLab token: glpat-xxxxxxxxxxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("glpat-"));
    }

    #[test]
    fn only_whitespace() {
        assert_eq!(redact_secrets("   \n\t  "), "   \n\t  ");
    }

    #[test]
    fn secret_at_end_of_line() {
        let text = "token: sk-abc123";
        let result = redact_secrets(text);
        assert_eq!(result, "token: [REDACTED]");
    }

    #[test]
    fn redacts_secret_in_url() {
        let text = "https://api.example.com?key=sk-abc123xyz";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123xyz"));
    }

    #[test]
    fn redacts_secret_in_json() {
        let text = r#"{"api_key":"sk-abc123def456"}"#;
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123def456"));
    }

    #[test]
    fn sanitize_home_path() {
        let text = "error at /home/user/project/src/main.rs:42";
        let result = sanitize_paths(text);
        assert_eq!(result, "error at [PATH]");
    }

    #[test]
    fn sanitize_users_path() {
        let text = "failed: /Users/dev/code/lib.rs not found";
        let result = sanitize_paths(text);
        assert!(result.contains("[PATH]"));
        assert!(!result.contains("/Users/"));
    }

    #[test]
    fn sanitize_no_paths() {
        let text = "normal error message";
        let result = sanitize_paths(text);
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn redacts_huggingface_token() {
        let text = "HuggingFace token: hf_abcdefghijklmnopqrstuvwxyz";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("hf_"));
    }

    #[test]
    fn redacts_npm_token() {
        let text = "NPM token npm_abc123XYZ";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("npm_abc"));
    }

    #[test]
    fn redacts_docker_pat() {
        let text = "Docker token: dckr_pat_xxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("dckr_pat_"));
    }

    use proptest::prelude::*;

    #[test]
    fn scrub_no_match_passthrough() {
        let text = "hello world, nothing sensitive here";
        let result = scrub_content(text);
        assert_matches!(result, Cow::Borrowed(_));
        assert_eq!(result.as_ref(), text);
    }

    #[test]
    fn scrub_only_secrets() {
        let text = "key: sk-abc123def";
        let result = scrub_content(text);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123"));
        assert!(!result.contains("/home/"));
    }

    #[test]
    fn scrub_only_paths() {
        let text = "error at /Users/dev/project/src/main.rs:42";
        let result = scrub_content(text);
        assert!(result.contains("[PATH]"));
        assert!(!result.contains("/Users/dev/"));
    }

    #[test]
    fn scrub_secrets_and_paths_combined() {
        let text = "token sk-abc123 found at /home/user/config.toml";
        let result = scrub_content(text);
        assert!(result.contains("[REDACTED]"));
        assert!(result.contains("[PATH]"));
        assert!(!result.contains("sk-abc123"));
        assert!(!result.contains("/home/user/"));
    }

    #[test]
    fn scrub_secrets_no_paths() {
        // Secret found but no path → function returns Cow::Owned (modified string)
        let text = "use sk-abc123 for auth";
        let result = scrub_content(text);
        assert!(
            matches!(result, Cow::Owned(_)),
            "must return Cow::Owned when secret was found"
        );
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("[PATH]"));
    }

    #[test]
    fn sanitize_paths_all_prefixes() {
        let cases = [
            ("/root/secrets.toml", "/root/"),
            ("/tmp/tmpfile.lock", "/tmp/"),
            ("/var/log/app.log", "/var/"),
        ];
        for (text, prefix) in cases {
            let result = sanitize_paths(text);
            assert!(result.contains("[PATH]"), "{prefix} must be sanitized");
            assert!(
                !result.contains(prefix),
                "{prefix} must be removed from output"
            );
        }
    }

    // ── #5917: Bearer/JWT coverage added to redact_secrets/scrub_content ──────────────

    #[test]
    fn redacts_bearer_token() {
        let result = redact_secrets("Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(
            result.contains("[REDACTED]"),
            "Bearer token must be redacted: {result}"
        );
        assert!(
            !result.contains("eyJhbGciOiJSUzI1NiJ9"),
            "raw JWT header must not appear: {result}"
        );
        assert!(
            result.contains("Authorization:"),
            "header name must be preserved: {result}"
        );
    }

    #[test]
    fn redacts_bearer_token_case_insensitive() {
        let result = redact_secrets("authorization: bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(
            result.contains("[REDACTED]"),
            "Bearer header match must be case-insensitive: {result}"
        );
    }

    #[test]
    fn redacts_standalone_jwt() {
        let jwt = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIn0.SflKxwRJSMeKKF2";
        let input = format!("token value: {jwt} was found in logs");
        let result = redact_secrets(&input);
        assert!(
            result.contains("[REDACTED_JWT]"),
            "standalone JWT must be replaced with [REDACTED_JWT]: {result}"
        );
        assert!(
            !result.contains("eyJhbGci"),
            "raw JWT must not appear: {result}"
        );
    }

    #[test]
    fn redacts_alg_none_jwt_with_empty_signature() {
        let input = "token: eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyIn0. was submitted";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED_JWT]"),
            "alg=none JWT with empty signature must be redacted: {result}"
        );
    }

    #[test]
    fn scrub_content_redacts_secret_path_bearer_and_jwt_together() {
        let text =
            "key sk-abc123 at /home/user/f with Authorization: Bearer eyJhbG.pay.sig and eyJx.b.c";
        let result = scrub_content(text);
        assert!(result.contains("[REDACTED]"), "API key must be redacted");
        assert!(result.contains("[PATH]"), "path must be redacted");
        assert!(!result.contains("sk-abc123"), "raw API key must not appear");
        assert!(!result.contains("eyJhbG"), "raw JWT must not appear");
    }

    proptest! {
        #[test]
        fn redact_secrets_never_panics(s in ".*") {
            let _ = redact_secrets(&s);
        }

        #[test]
        fn sanitize_paths_never_panics(s in ".*") {
            let _ = sanitize_paths(&s);
        }

        #[test]
        fn redact_preserves_non_secret_text(s in "[a-zA-Z0-9 .,!?]{1,200}") {
            // Only test strings that genuinely contain nothing redact_secrets will touch:
            // no known secret prefix, no "eyJ" (JWT marker), no case-insensitive "bearer".
            let has_secret_prefix = SECRET_PREFIXES.iter().any(|p| s.contains(*p));
            let has_jwt_marker = s.contains("eyJ");
            let has_bearer_marker = s.to_lowercase().contains("bearer");
            if !has_secret_prefix && !has_jwt_marker && !has_bearer_marker {
                let result = redact_secrets(&s);
                assert_eq!(result.as_ref(), s.as_str());
            }
        }

        #[test]
        fn scrub_content_never_panics(s in ".*") {
            let _ = scrub_content(&s);
        }

        #[test]
        fn scrub_content_result_never_contains_raw_secret(s in ".*") {
            let result = scrub_content(&s);
            for prefix in SECRET_PREFIXES {
                assert!(
                    !result.contains(*prefix),
                    "scrub_content must redact prefix: {prefix}"
                );
            }
        }
    }
}
