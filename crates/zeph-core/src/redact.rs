use std::borrow::Cow;

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
];

/// Replace tokens containing known secret patterns with `[REDACTED]`.
///
/// Preserves all original whitespace (newlines, tabs, indentation).
/// Returns `Cow::Borrowed` when no secrets found (zero-allocation fast path).
#[must_use]
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    if !SECRET_PREFIXES.iter().any(|p| text.contains(p)) {
        return Cow::Borrowed(text);
    }

    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        if bytes[i].is_ascii_whitespace() {
            result.push(bytes[i] as char);
            i += 1;
        } else {
            let start = i;
            while i < len && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let token = &text[start..i];
            if SECRET_PREFIXES.iter().any(|prefix| token.contains(prefix)) {
                result.push_str("[REDACTED]");
            } else {
                result.push_str(token);
            }
        }
    }

    Cow::Owned(result)
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
}
