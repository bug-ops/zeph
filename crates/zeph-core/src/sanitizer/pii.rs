// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! PII filter: regex-based scrubber for email, phone, SSN, and credit card numbers.
//!
//! Applied to tool outputs before they enter LLM context and before debug dumps are written.
//! Configured under `[security.pii_filter]` in the agent config file.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

pub use zeph_config::{CustomPiiPattern, PiiFilterConfig};

// ---------------------------------------------------------------------------
// Built-in patterns
// ---------------------------------------------------------------------------

/// Email: tightened to reduce false positives on code patterns.
///
/// - TLD restricted to 2-6 alpha chars
/// - Local part minimum 2 chars, restricted to `[a-zA-Z0-9._%+-]`
/// - Domain labels must be purely alphabetic (rejects `@v2.config`, `@2host.io`,
///   `@office365.com`). This is intentionally strict: the PII filter prefers
///   false negatives over false positives on tool output content.
/// - Rejects `@localhost` (no dot in domain)
///
/// Known limitation: purely-alphabetic code-style patterns such as
/// `decorator@factory.method` are not rejected because they are
/// indistinguishable from a real hostname without a TLD allowlist.
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z0-9._%+\-]{2,}@(?:[a-zA-Z]+\.)+[a-zA-Z]{2,6}").expect("valid EMAIL_RE")
});

/// US phone numbers, optional country code.
static PHONE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b").expect("valid PHONE_RE")
});

/// US Social Security Number (NNN-NN-NNNN).
static SSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("valid SSN_RE"));

/// Credit card number: 16 digits in groups of 4 (space or dash separated, or bare).
static CREDIT_CARD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d{4}[-\s]?){3}\d{4}\b").expect("valid CREDIT_CARD_RE"));

// ---------------------------------------------------------------------------
// Internal pattern record
// ---------------------------------------------------------------------------

struct PiiPattern {
    regex: Regex,
    replacement: &'static str,
}

struct CustomPiiPatternCompiled {
    regex: Regex,
    replacement: String,
}

// ---------------------------------------------------------------------------
// PiiFilter
// ---------------------------------------------------------------------------

/// Stateless PII filter. Construct once from [`PiiFilterConfig`] and store on the agent.
///
/// When disabled, all methods are no-ops that return the input unchanged.
pub struct PiiFilter {
    enabled: bool,
    /// Built-in patterns selected by config flags.
    builtin: Vec<PiiPattern>,
    /// User-defined patterns from `custom_patterns`.
    custom: Vec<CustomPiiPatternCompiled>,
}

impl PiiFilter {
    /// Construct a new filter from the given configuration.
    ///
    /// Custom pattern compilation errors are logged as warnings; invalid patterns are skipped.
    #[must_use]
    pub fn new(config: PiiFilterConfig) -> Self {
        let mut builtin = Vec::new();
        if config.filter_email {
            builtin.push(PiiPattern {
                regex: EMAIL_RE.clone(),
                replacement: "[PII:email]",
            });
        }
        if config.filter_phone {
            builtin.push(PiiPattern {
                regex: PHONE_RE.clone(),
                replacement: "[PII:phone]",
            });
        }
        if config.filter_ssn {
            builtin.push(PiiPattern {
                regex: SSN_RE.clone(),
                replacement: "[PII:ssn]",
            });
        }
        if config.filter_credit_card {
            builtin.push(PiiPattern {
                regex: CREDIT_CARD_RE.clone(),
                replacement: "[PII:credit_card]",
            });
        }

        let mut custom = Vec::new();
        for p in config.custom_patterns {
            match RegexBuilder::new(&p.pattern)
                .size_limit(1_000_000)
                .dfa_size_limit(1_000_000)
                .build()
            {
                Ok(regex) => custom.push(CustomPiiPatternCompiled {
                    regex,
                    replacement: p.replacement,
                }),
                Err(e) => {
                    tracing::warn!(name = %p.name, error = %e, "PII filter: skipping invalid custom pattern");
                }
            }
        }

        Self {
            enabled: config.enabled,
            builtin,
            custom,
        }
    }

    /// Scrub PII from `text`.
    ///
    /// Returns `Cow::Borrowed` when no PII is found (zero-alloc fast path).
    /// Each match is replaced with a category label such as `[PII:email]`.
    ///
    /// When the filter is disabled, always returns `Cow::Borrowed(text)`.
    #[must_use]
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if !self.enabled || (self.builtin.is_empty() && self.custom.is_empty()) {
            return Cow::Borrowed(text);
        }

        let mut result: Option<String> = None;

        for p in &self.builtin {
            let current: &str = result.as_deref().unwrap_or(text);
            let replaced = p.regex.replace_all(current, p.replacement);
            if let Cow::Owned(s) = replaced {
                result = Some(s);
            }
        }

        for p in &self.custom {
            let current: &str = result.as_deref().unwrap_or(text);
            let replaced = p.regex.replace_all(current, p.replacement.as_str());
            if let Cow::Owned(s) = replaced {
                result = Some(s);
            }
        }

        match result {
            Some(s) => Cow::Owned(s),
            None => Cow::Borrowed(text),
        }
    }

    /// Check whether `text` contains any PII without performing replacement.
    ///
    /// Returns `false` when the filter is disabled.
    #[must_use]
    pub fn has_pii(&self, text: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.builtin.iter().any(|p| p.regex.is_match(text))
            || self.custom.iter().any(|p| p.regex.is_match(text))
    }

    /// Returns `true` if the filter is enabled and has at least one active pattern.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled && (!self.builtin.is_empty() || !self.custom.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_all() -> PiiFilter {
        PiiFilter::new(PiiFilterConfig {
            enabled: true,
            ..PiiFilterConfig::default()
        })
    }

    fn filter_disabled() -> PiiFilter {
        PiiFilter::new(PiiFilterConfig::default())
    }

    // --- disabled fast-path ---

    #[test]
    fn disabled_returns_borrowed() {
        let f = filter_disabled();
        let text = "email: user@example.com";
        let result = f.scrub(text);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, text);
    }

    #[test]
    fn disabled_has_pii_false() {
        let f = filter_disabled();
        assert!(!f.has_pii("user@example.com"));
    }

    // --- email ---

    #[test]
    fn scrubs_email() {
        let f = filter_all();
        let result = f.scrub("contact us at user@example.com please");
        assert_eq!(result, "contact us at [PII:email] please");
    }

    #[test]
    fn scrubs_tagged_email() {
        let f = filter_all();
        let result = f.scrub("user+tag@sub.domain.org is the address");
        assert_eq!(result, "[PII:email] is the address");
    }

    #[test]
    fn does_not_match_at_localhost() {
        let f = filter_all();
        let text = "user@localhost should not match";
        let result = f.scrub(text);
        assert_eq!(result, text, "user@localhost must not be matched");
    }

    #[test]
    fn does_not_match_versioned_domain() {
        let f = filter_all();
        // @v2.config — domain label 'v2' starts with a digit, not a letter.
        let text = "template@v2.config should not match";
        let result = f.scrub(text);
        assert_eq!(
            result, text,
            "v2.config must not be detected as email domain"
        );
    }

    #[test]
    fn does_not_match_db_at_localhost() {
        let f = filter_all();
        let text = "connect to db@localhost:5432";
        let result = f.scrub(text);
        // @localhost has no dot in the domain part, so the pattern won't match
        assert!(
            !result.contains("[PII:email]"),
            "localhost must not be detected as email: {result}"
        );
    }

    #[test]
    fn does_not_match_short_local() {
        let f = filter_all();
        // single-char local part (a@b.co) — local part must be 2+ chars
        let text = "a@b.co";
        let result = f.scrub(text);
        assert_eq!(result, text, "single-char local part must not match");
    }

    // --- phone ---

    #[test]
    fn scrubs_us_phone() {
        let f = filter_all();
        let result = f.scrub("call 555-867-5309 for info");
        assert_eq!(result, "call [PII:phone] for info");
    }

    #[test]
    fn scrubs_us_phone_with_country_code() {
        let f = filter_all();
        let result = f.scrub("call +1-800-555-1234 now");
        // The regex uses \b which won't anchor before '+', so '+' is left behind.
        assert_eq!(result, "call +[PII:phone] now");
    }

    // --- SSN ---

    #[test]
    fn scrubs_ssn() {
        let f = filter_all();
        let result = f.scrub("SSN: 123-45-6789 on file");
        assert_eq!(result, "SSN: [PII:ssn] on file");
    }

    // --- credit card ---

    #[test]
    fn scrubs_credit_card() {
        let f = filter_all();
        let result = f.scrub("card: 4111 1111 1111 1111 expired");
        assert_eq!(result, "card: [PII:credit_card] expired");
    }

    #[test]
    fn scrubs_credit_card_dashes() {
        let f = filter_all();
        let result = f.scrub("card 4111-1111-1111-1111");
        assert_eq!(result, "card [PII:credit_card]");
    }

    // --- no PII ---

    #[test]
    fn no_pii_returns_borrowed() {
        let f = filter_all();
        let text = "no sensitive data here";
        let result = f.scrub(text);
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    // --- has_pii ---

    #[test]
    fn has_pii_detects_email() {
        let f = filter_all();
        assert!(f.has_pii("reach user@example.com"));
        assert!(!f.has_pii("no pii here"));
    }

    // --- custom patterns ---

    #[test]
    fn custom_pattern_scrubs() {
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            filter_email: false,
            filter_phone: false,
            filter_ssn: false,
            filter_credit_card: false,
            custom_patterns: vec![CustomPiiPattern {
                name: "employee_id".to_owned(),
                pattern: r"EMP-\d{6}".to_owned(),
                replacement: "[PII:employee_id]".to_owned(),
            }],
        });
        let result = f.scrub("ID: EMP-123456 assigned");
        assert_eq!(result, "ID: [PII:employee_id] assigned");
    }

    #[test]
    fn invalid_custom_pattern_skipped() {
        // Should not panic — invalid regex is logged and skipped.
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            custom_patterns: vec![CustomPiiPattern {
                name: "bad".to_owned(),
                pattern: r"[invalid(".to_owned(),
                replacement: "[PII:bad]".to_owned(),
            }],
            ..PiiFilterConfig::default()
        });
        // Filter still works with built-in patterns
        let result = f.scrub("user@example.com");
        assert_eq!(result, "[PII:email]");
    }

    // --- empty input ---

    #[test]
    fn empty_input_returns_borrowed() {
        let f = filter_all();
        let result = f.scrub("");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "");
    }

    // --- multiple PII types in one string ---

    #[test]
    fn scrubs_multiple_pii_types() {
        let f = filter_all();
        let input = "Email: user@example.com, SSN: 123-45-6789";
        let result = f.scrub(input);
        assert!(
            result.contains("[PII:email]"),
            "email must be scrubbed: {result}"
        );
        assert!(
            result.contains("[PII:ssn]"),
            "SSN must be scrubbed: {result}"
        );
        assert!(
            !result.contains("user@example.com"),
            "raw email must not remain"
        );
        assert!(!result.contains("123-45-6789"), "raw SSN must not remain");
    }

    // --- unicode text without PII ---

    #[test]
    fn unicode_no_pii_returns_borrowed() {
        let f = filter_all();
        let text = "Привет мир, no PII here — €100";
        let result = f.scrub(text);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "unicode text without PII must be Borrowed"
        );
    }

    // --- is_enabled ---

    #[test]
    fn is_enabled_true_when_enabled_with_patterns() {
        let f = filter_all();
        assert!(f.is_enabled());
    }

    #[test]
    fn is_enabled_false_when_disabled() {
        let f = filter_disabled();
        assert!(!f.is_enabled());
    }

    #[test]
    fn is_enabled_false_when_all_builtin_off_and_no_custom() {
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            filter_email: false,
            filter_phone: false,
            filter_ssn: false,
            filter_credit_card: false,
            custom_patterns: vec![],
        });
        assert!(!f.is_enabled());
    }

    // --- selective category disable ---

    #[test]
    fn selective_email_only() {
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            filter_email: true,
            filter_phone: false,
            filter_ssn: false,
            filter_credit_card: false,
            custom_patterns: vec![],
        });
        let result = f.scrub("user@example.com and 555-867-5309");
        assert!(result.contains("[PII:email]"), "email scrubbed");
        assert!(
            result.contains("555-867-5309"),
            "phone must NOT be scrubbed when disabled"
        );
    }

    // --- has_pii with custom pattern ---

    #[test]
    fn has_pii_detects_custom_pattern() {
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            filter_email: false,
            filter_phone: false,
            filter_ssn: false,
            filter_credit_card: false,
            custom_patterns: vec![CustomPiiPattern {
                name: "token".to_owned(),
                pattern: r"TOKEN-\d+".to_owned(),
                replacement: "[PII:token]".to_owned(),
            }],
        });
        assert!(f.has_pii("auth TOKEN-42 used"));
        assert!(!f.has_pii("no token here"));
    }

    // --- credit card bare (no separators) ---

    #[test]
    fn scrubs_credit_card_bare() {
        let f = filter_all();
        let result = f.scrub("card 4111111111111111 end");
        assert!(
            result.contains("[PII:credit_card]"),
            "bare 16-digit CC must be scrubbed: {result}"
        );
    }

    // --- SSN false positive: dates should not match ---

    #[test]
    fn does_not_scrub_date_as_ssn() {
        let f = PiiFilter::new(PiiFilterConfig {
            enabled: true,
            filter_ssn: true,
            filter_email: false,
            filter_phone: false,
            filter_credit_card: false,
            custom_patterns: vec![],
        });
        // A date like 12-01-2024 has the form DDD-DD-DDDD but \b\d{3}-\d{2}-\d{4}\b
        // matches exactly 3-2-4 digits. "12-01-2024" is 2-2-4, so it must NOT match.
        let text = "date 12-01-2024 passed";
        let result = f.scrub(text);
        assert_eq!(result, text, "date DD-MM-YYYY must not be detected as SSN");
    }
}
