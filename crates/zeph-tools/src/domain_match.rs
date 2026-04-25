// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Domain pattern matching used by both the web scrape allowlist/denylist and the
//! sandbox egress deny list.
//!
//! Patterns support:
//! - Exact hostname: `"example.com"` matches only `"example.com"`.
//! - Single-level wildcard: `"*.example.com"` matches `"sub.example.com"` but not
//!   `"example.com"` or `"deep.sub.example.com"`.
//!
//! Patterns with multiple `*` segments are treated as exact strings.

/// Returns `true` if `host` matches `pattern`.
///
/// Matching rules:
/// - `"example.com"` → exact match only.
/// - `"*.example.com"` → single-subdomain match: `sub.example.com` matches,
///   `example.com` and `a.b.example.com` do not.
/// - Patterns with more than one `*` are treated as exact strings (not supported as wildcards).
///
/// # Examples
///
/// ```
/// use zeph_tools::domain_match::domain_matches;
///
/// assert!(domain_matches("example.com", "example.com"));
/// assert!(!domain_matches("example.com", "sub.example.com"));
/// assert!(domain_matches("*.example.com", "sub.example.com"));
/// assert!(!domain_matches("*.example.com", "example.com"));
/// assert!(!domain_matches("*.example.com", "a.b.example.com"));
/// ```
#[must_use]
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    if pattern.starts_with("*.") {
        // Allow only a single subdomain level: `*.example.com` → `<label>.example.com`
        let suffix = &pattern[1..]; // ".example.com"
        if let Some(remainder) = host.strip_suffix(suffix) {
            // remainder must be a single DNS label (no dots, non-empty)
            !remainder.is_empty() && !remainder.contains('.')
        } else {
            false
        }
    } else {
        pattern == host
    }
}

/// Validate a list of domain patterns for use in `denied_domains` or allowlists.
///
/// Each entry must match `^[a-zA-Z0-9.*-]+$`. Returns an error describing the
/// first invalid entry, or `Ok(())` when all entries are valid.
///
/// # Errors
///
/// Returns a descriptive error string when any pattern contains invalid characters
/// (spaces, slashes, colons, or other characters outside the allowed set).
pub fn validate_domain_patterns(patterns: &[String]) -> Result<(), String> {
    for pattern in patterns {
        if !pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '*' | '-'))
        {
            return Err(format!(
                "invalid domain pattern {pattern:?}: only alphanumeric characters, dots, \
                 hyphens, and a leading wildcard '*' are allowed"
            ));
        }
        if pattern.is_empty() {
            return Err("empty domain pattern is not allowed".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("example.com", "other.com"));
        assert!(!domain_matches("example.com", "sub.example.com"));
    }

    #[test]
    fn wildcard_single_subdomain() {
        assert!(domain_matches("*.example.com", "sub.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(!domain_matches("*.example.com", "a.b.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_empty_label() {
        assert!(!domain_matches("*.example.com", ".example.com"));
    }

    #[test]
    fn multi_wildcard_treated_as_exact() {
        assert!(!domain_matches("*.*.example.com", "a.b.example.com"));
    }

    #[test]
    fn validate_accepts_valid_patterns() {
        let patterns = vec![
            "example.com".to_owned(),
            "*.pastebin.com".to_owned(),
            "my-host.co.uk".to_owned(),
        ];
        assert!(validate_domain_patterns(&patterns).is_ok());
    }

    #[test]
    fn validate_rejects_spaces() {
        let patterns = vec!["bad domain.com".to_owned()];
        assert!(validate_domain_patterns(&patterns).is_err());
    }

    #[test]
    fn validate_rejects_slashes() {
        let patterns = vec!["example.com/path".to_owned()];
        assert!(validate_domain_patterns(&patterns).is_err());
    }

    #[test]
    fn validate_rejects_empty() {
        let patterns = vec![String::new()];
        assert!(validate_domain_patterns(&patterns).is_err());
    }
}
