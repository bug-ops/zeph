// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Sanitize a user query string for safe FTS usage.
///
/// `SQLite`: strip FTS5 special characters by splitting on non-alphanumeric
/// characters and joining with spaces. This prevents syntax errors in
/// `MATCH` clauses from special FTS5 operators.
///
/// `PostgreSQL`: `plainto_tsquery` handles most sanitization; strip obvious
/// injection attempts (single quotes).
#[must_use]
#[cfg(feature = "sqlite")]
pub fn sanitize_fts_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn sanitize_fts_query(query: &str) -> String {
    query.replace('\'', "''")
}

#[cfg(test)]
#[cfg(feature = "sqlite")]
mod tests {
    use super::*;

    #[test]
    fn strips_fts5_special_chars() {
        assert_eq!(sanitize_fts_query("skill-audit"), "skill audit");
        assert_eq!(sanitize_fts_query("hello, world"), "hello world");
        assert_eq!(sanitize_fts_query("a+b*c^d"), "a b c d");
    }

    #[test]
    fn preserves_alphanumeric_tokens() {
        assert_eq!(sanitize_fts_query("hello world"), "hello world");
        assert_eq!(sanitize_fts_query("abc123"), "abc123");
    }
}
