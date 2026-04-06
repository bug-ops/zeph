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
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
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

// ── Messages FTS helpers ─────────────────────────────────────────────────────

/// `WHERE` clause fragment for messages FTS keyword search.
///
/// The placeholder binds the sanitized query string.
///
/// `SQLite`: `messages_fts MATCH ?`
/// `PostgreSQL`: `m.tsv @@ plainto_tsquery('english', $1)`
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn messages_fts_where() -> &'static str {
    "messages_fts MATCH ?"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn messages_fts_where() -> &'static str {
    "m.tsv @@ plainto_tsquery('english', $1)"
}

/// `JOIN` clause needed for messages FTS (empty for `PostgreSQL` — tsv lives on the table).
///
/// `SQLite`: `JOIN messages_fts f ON f.rowid = m.id`
/// `PostgreSQL`: empty string (tsv column is on messages directly)
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn messages_fts_join() -> &'static str {
    "JOIN messages_fts f ON f.rowid = m.id"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn messages_fts_join() -> &'static str {
    ""
}

/// Rank expression for messages FTS used in `SELECT` list.
///
/// `SQLite`: `-rank AS score` (FTS5 `rank` is negative; negate for positive-is-better)
/// `PostgreSQL`: `ts_rank(m.tsv, plainto_tsquery('english', $1)) AS score`
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn messages_fts_rank_select() -> &'static str {
    "-rank AS score"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn messages_fts_rank_select() -> &'static str {
    "ts_rank(m.tsv, plainto_tsquery('english', $1)) AS score"
}

/// `ORDER BY` direction for messages FTS rank.
///
/// `SQLite`: `rank` (ascending — FTS5 rank is negative so ASC = best first)
/// `PostgreSQL`: `score DESC` (`ts_rank` is positive so DESC = best first)
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn messages_fts_order_by() -> &'static str {
    "rank"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn messages_fts_order_by() -> &'static str {
    "score DESC"
}

// ── Graph entities FTS helpers ───────────────────────────────────────────────

/// `WHERE` clause fragment for graph entity FTS search (basic, no ranking).
///
/// `SQLite`: `graph_entities_fts MATCH ?`
/// `PostgreSQL`: `e.tsv @@ plainto_tsquery('english', $1)`
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn graph_entities_fts_where() -> &'static str {
    "graph_entities_fts MATCH ?"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn graph_entities_fts_where() -> &'static str {
    "e.tsv @@ plainto_tsquery('english', $1)"
}

/// `JOIN` clause needed for graph entities FTS.
///
/// `SQLite`: `JOIN graph_entities_fts fts ON fts.rowid = e.id`
/// `PostgreSQL`: empty string (tsv column lives on `graph_entities`)
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn graph_entities_fts_join() -> &'static str {
    "JOIN graph_entities_fts fts ON fts.rowid = e.id"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn graph_entities_fts_join() -> &'static str {
    ""
}

/// Weighted rank expression for ranked graph entity FTS (`SELECT` list).
///
/// `SQLite`: `-bm25(graph_entities_fts, 10.0, 1.0) AS fts_rank`
///   (bm25 is negative; negate to get positive-is-better; 10x weight for name)
/// `PostgreSQL`: `ts_rank(ARRAY[0,0,1,10], e.tsv, plainto_tsquery('english', $1)) AS fts_rank`
///   (weight array: [D, C, B, A]; A=name weight 10, B=summary weight 1)
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn graph_entities_fts_rank_select() -> &'static str {
    "-bm25(graph_entities_fts, 10.0, 1.0) AS fts_rank"
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn graph_entities_fts_rank_select() -> &'static str {
    "ts_rank(ARRAY[0.0::float4,0.0::float4,1.0::float4,10.0::float4], e.tsv, plainto_tsquery('english', $1)) AS fts_rank"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    mod sqlite {
        use super::*;

        #[test]
        fn sanitize_strips_fts5_special_chars() {
            assert_eq!(sanitize_fts_query("skill-audit"), "skill audit");
            assert_eq!(sanitize_fts_query("hello, world"), "hello world");
            assert_eq!(sanitize_fts_query("a+b*c^d"), "a b c d");
        }

        #[test]
        fn sanitize_preserves_alphanumeric_tokens() {
            assert_eq!(sanitize_fts_query("hello world"), "hello world");
            assert_eq!(sanitize_fts_query("abc123"), "abc123");
        }

        #[test]
        fn messages_fts_helpers() {
            assert!(messages_fts_where().contains("MATCH"));
            assert!(messages_fts_join().contains("messages_fts"));
            assert_eq!(messages_fts_rank_select(), "-rank AS score");
            assert_eq!(messages_fts_order_by(), "rank");
        }

        #[test]
        fn graph_entities_fts_helpers() {
            assert!(graph_entities_fts_where().contains("MATCH"));
            assert!(graph_entities_fts_join().contains("graph_entities_fts"));
            assert!(graph_entities_fts_rank_select().contains("bm25"));
        }
    }

    #[cfg(feature = "postgres")]
    mod postgres {
        use super::*;

        #[test]
        fn sanitize_escapes_single_quotes() {
            assert_eq!(sanitize_fts_query("it's"), "it''s");
            assert_eq!(sanitize_fts_query("hello world"), "hello world");
        }

        #[test]
        fn messages_fts_helpers() {
            assert!(messages_fts_where().contains("plainto_tsquery"));
            assert_eq!(messages_fts_join(), "");
            assert!(messages_fts_rank_select().contains("ts_rank"));
            assert_eq!(messages_fts_order_by(), "score DESC");
        }

        #[test]
        fn graph_entities_fts_helpers() {
            assert!(graph_entities_fts_where().contains("plainto_tsquery"));
            assert_eq!(graph_entities_fts_join(), "");
            assert!(graph_entities_fts_rank_select().contains("ts_rank"));
        }
    }
}
