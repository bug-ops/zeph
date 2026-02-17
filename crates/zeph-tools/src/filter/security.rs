use std::sync::LazyLock;

use regex::Regex;

static SECURITY_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"warning:.*unused.*Result",
        r"warning:.*must be used",
        r"thread '.*' panicked at",
        r"warning:.*unsafe",
        r"dereference of raw pointer",
        r"(?i)authentication failed",
        r"(?i)unauthorized",
        r"(?i)permission denied",
        r"(?i)(401|403)\s+(Unauthorized|Forbidden)",
        r"(?i)weak cipher",
        r"(?i)deprecated algorithm",
        r"(?i)insecure hash",
        r"(?i)SQL injection",
        r"(?i)unsafe query",
        r"RUSTSEC-\d{4}-\d{4}",
        r"(?i)security advisory",
        r"(?i)vulnerability detected",
    ]
    .iter()
    .map(|s| Regex::new(s).unwrap())
    .collect()
});

/// Pre-compile extra security patterns from user config strings.
#[must_use]
pub fn compile_extra_patterns(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|s| match Regex::new(s) {
            Ok(re) => Some(re),
            Err(e) => {
                tracing::warn!(pattern = %s, error = %e, "invalid security extra_pattern, skipping");
                None
            }
        })
        .collect()
}

#[must_use]
pub fn extract_security_lines<'a>(text: &'a str, extra: &[Regex]) -> Vec<&'a str> {
    text.lines()
        .filter(|line| {
            SECURITY_PATTERNS.iter().any(|pat| pat.is_match(line))
                || extra.iter().any(|pat| pat.is_match(line))
        })
        .collect()
}

pub fn append_security_warnings(filtered: &mut String, raw_output: &str, extra: &[Regex]) {
    let security_lines = extract_security_lines(raw_output, extra);
    if security_lines.is_empty() {
        return;
    }
    filtered.push_str("\n\n--- Security Warnings (preserved) ---\n");
    for line in &security_lines {
        filtered.push_str(line);
        filtered.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_panic() {
        let lines = extract_security_lines("thread 'main' panicked at 'oops'\nnormal line", &[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("panicked"));
    }

    #[test]
    fn detects_rustsec() {
        let lines = extract_security_lines("RUSTSEC-2024-0001 advisory here", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_auth_failure() {
        let lines = extract_security_lines("Error: Authentication failed for user admin", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_permission_denied() {
        let lines = extract_security_lines("Permission denied (publickey)", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_http_status_codes() {
        let lines = extract_security_lines("HTTP 401 Unauthorized", &[]);
        assert_eq!(lines.len(), 1);
        let lines = extract_security_lines("HTTP 403 Forbidden", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_sql_injection() {
        let lines = extract_security_lines("WARNING: potential SQL injection detected", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_unsafe_warnings() {
        let lines = extract_security_lines("warning: use of unsafe block in function foo", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_vulnerability() {
        let lines = extract_security_lines("vulnerability detected in dep xyz", &[]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn detects_weak_crypto() {
        let lines = extract_security_lines(
            "weak cipher suite selected\ninsecure hash MD5 used\ndeprecated algorithm RC4",
            &[],
        );
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn no_false_positives() {
        let lines = extract_security_lines(
            "Compiling zeph v0.9.0\nFinished dev [unoptimized] target(s) in 2.3s",
            &[],
        );
        assert!(lines.is_empty());
    }

    #[test]
    fn extra_patterns_work() {
        let extra = compile_extra_patterns(&["TODO: security review".to_owned()]);
        let lines = extract_security_lines("TODO: security review needed here", &extra);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn compile_extra_warns_on_invalid() {
        let extra = compile_extra_patterns(&["valid".to_owned(), "[invalid".to_owned()]);
        assert_eq!(extra.len(), 1);
    }

    #[test]
    fn append_does_nothing_on_clean_output() {
        let mut filtered = "clean output".to_owned();
        append_security_warnings(&mut filtered, "no warnings here", &[]);
        assert_eq!(filtered, "clean output");
    }

    #[test]
    fn append_adds_security_section() {
        let mut filtered = "filtered result".to_owned();
        append_security_warnings(&mut filtered, "thread 'main' panicked at 'oops'", &[]);
        assert!(filtered.contains("--- Security Warnings (preserved) ---"));
        assert!(filtered.contains("panicked"));
    }

    #[test]
    fn integration_filter_removes_security_restored() {
        let raw = "normal output\nthread 'main' panicked at 'assertion failed'\nmore normal";
        let mut filtered = "normal output\nmore normal".to_owned();
        append_security_warnings(&mut filtered, raw, &[]);
        assert!(filtered.contains("panicked"));
    }
}
