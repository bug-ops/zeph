// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Sanitise a user-supplied task prompt before injecting it into the agent loop.
///
/// Applies two transformations in order:
///
/// 1. **Truncation** — caps the output at 512 Unicode code points. Truncation is
///    code-point–safe and will not produce invalid UTF-8.
/// 2. **Control-character stripping** — removes characters with code points below
///    `U+0020`, except `\n` (U+000A) and `\t` (U+0009) which are preserved.
///
/// # Examples
///
/// ```
/// use zeph_scheduler::sanitize_task_prompt;
///
/// // Control characters are stripped.
/// assert_eq!(sanitize_task_prompt("hello\x01world"), "helloworld");
///
/// // Newlines and tabs are preserved.
/// assert_eq!(sanitize_task_prompt("line1\nline2"), "line1\nline2");
///
/// // Long strings are truncated to 512 code points.
/// let long = "x".repeat(600);
/// assert_eq!(sanitize_task_prompt(&long).chars().count(), 512);
/// ```
#[must_use]
pub fn sanitize_task_prompt(s: &str) -> String {
    s.chars()
        .take(512)
        .filter(|&c| c >= '\x20' || c == '\n' || c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_chars() {
        assert_eq!(sanitize_task_prompt("hello\x01\x00world"), "helloworld");
    }

    #[test]
    fn preserves_newline_and_tab() {
        assert_eq!(
            sanitize_task_prompt("line1\nline2\ttab"),
            "line1\nline2\ttab"
        );
    }

    #[test]
    fn truncates_at_512_code_points() {
        let long = "a".repeat(1000);
        assert_eq!(sanitize_task_prompt(&long).chars().count(), 512);
    }

    #[test]
    fn handles_multibyte_boundary() {
        // 512 copies of a 3-byte char followed by ASCII — must not panic
        let s: String = "é".repeat(600);
        let result = sanitize_task_prompt(&s);
        assert_eq!(result.chars().count(), 512);
    }
}
