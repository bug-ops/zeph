use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

const SECRET_PREFIXES: &[&str] = &[
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
    "ya29\\.",
    "glpat-",
    "hf_",
    "npm_",
    "dckr_pat_",
];

// Matches any secret prefix followed by non-whitespace characters.
// Using alternation so a single pass covers all prefixes.
static SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = SECRET_PREFIXES.join("|");
    let full = format!("(?:{pattern})[^\\s\"'`,;{{}}\\[\\]]*");
    Regex::new(&full).expect("secret redaction regex is valid")
});

static PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:/home/|/Users/|/root/|/tmp/|/var/)[^\s"'`,;{}\[\]]*"#)
        .expect("path redaction regex is valid")
});

/// Replace tokens containing known secret patterns with `[REDACTED]`.
///
/// Detects secrets embedded in URLs, JSON values, and quoted strings.
/// Returns `Cow::Borrowed` when no secrets found (zero-allocation fast path).
#[must_use]
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    // Fast path: check for any prefix substring before running regex.
    let raw_prefixes = &[
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
    if !raw_prefixes.iter().any(|p| text.contains(p)) {
        return Cow::Borrowed(text);
    }

    let result = SECRET_REGEX.replace_all(text, "[REDACTED]");
    match result {
        Cow::Borrowed(_) => Cow::Borrowed(text),
        Cow::Owned(s) => Cow::Owned(s),
    }
}

/// Replace absolute filesystem paths with `[PATH]` to prevent information disclosure.
#[must_use]
pub fn sanitize_paths(text: &str) -> Cow<'_, str> {
    const PATH_PREFIXES: &[&str] = &["/home/", "/Users/", "/root/", "/tmp/", "/var/"];

    if !PATH_PREFIXES.iter().any(|p| text.contains(p)) {
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
        assert!(matches!(result, Cow::Borrowed(_)));
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
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn all_secret_prefixes_tested() {
        for prefix in &[
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
        ] {
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
        assert!(matches!(result, Cow::Borrowed(_)));
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
}
