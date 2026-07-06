// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared placeholder-substitution helper for prompt template builders.

/// Substitute `{key}` placeholders in `template` from an ordered list of `(key, value)` pairs.
pub(crate) fn render(template: &str, substitutions: &[(&str, &str)]) -> String {
    substitutions
        .iter()
        .fold(template.to_string(), |acc, (k, v)| {
            acc.replace(&format!("{{{k}}}"), v)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_all_keys() {
        let out = render(
            "Hello {name}, you are {age}",
            &[("name", "Alice"), ("age", "30")],
        );
        assert_eq!(out, "Hello Alice, you are 30");
    }

    #[test]
    fn render_leaves_unmatched_braces_untouched() {
        let out = render("Data: {payload}", &[("payload", r#"{"key": "value"}"#)]);
        assert_eq!(out, r#"Data: {"key": "value"}"#);
    }

    #[test]
    fn render_missing_key_is_noop() {
        let out = render("Hello {name}", &[]);
        assert_eq!(out, "Hello {name}");
    }
}
