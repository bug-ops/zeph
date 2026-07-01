// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// SQL fragments that differ between database backends.
///
/// Implemented by zero-sized marker types ([`Sqlite`], [`Postgres`]).
/// All associated constants are `&'static str` for zero-cost usage.
pub trait Dialect: Send + Sync + 'static {
    /// Auto-increment primary key DDL fragment.
    ///
    /// `SQLite`: `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// `PostgreSQL`: `BIGSERIAL PRIMARY KEY`
    const AUTO_PK: &'static str;

    /// `INSERT OR IGNORE` prefix for this backend.
    ///
    /// `SQLite`: `INSERT OR IGNORE`
    /// `PostgreSQL`: `INSERT` (pair with `CONFLICT_NOTHING` suffix)
    const INSERT_IGNORE: &'static str;

    /// Suffix for conflict-do-nothing semantics.
    ///
    /// `SQLite`: empty string (handled by `INSERT OR IGNORE` prefix)
    /// `PostgreSQL`: `ON CONFLICT DO NOTHING`
    const CONFLICT_NOTHING: &'static str;

    /// Case-insensitive collation suffix for `ORDER BY` / `WHERE` clauses.
    ///
    /// `SQLite`: `COLLATE NOCASE`
    /// `PostgreSQL`: empty string (use `ILIKE` or `LOWER()` instead)
    const COLLATE_NOCASE: &'static str;

    /// Current epoch seconds expression.
    ///
    /// `SQLite`: `unixepoch('now')`
    /// `PostgreSQL`: `EXTRACT(EPOCH FROM NOW())::BIGINT`
    const EPOCH_NOW: &'static str;

    /// Case-insensitive comparison expression for a column.
    ///
    /// `SQLite`: `{col} COLLATE NOCASE`
    /// `PostgreSQL`: `LOWER({col})`
    fn ilike(col: &str) -> String;

    /// Epoch seconds expression for a timestamp column.
    ///
    /// Wraps the column in the backend-specific function that converts a stored
    /// timestamp to a Unix epoch integer, coalescing `NULL` to `0`.
    ///
    /// `SQLite`: `COALESCE(CAST(strftime('%s', {col}) AS INTEGER), 0)`
    /// `PostgreSQL`: `COALESCE(CAST(EXTRACT(EPOCH FROM {col}) AS BIGINT), 0)`
    fn epoch_from_col(col: &str) -> String;

    /// Cast suffix for a bind parameter carrying pre-serialized JSON text, destined
    /// for a JSON-typed column.
    ///
    /// `sqlx` sends string bind parameters as `TEXT`/`VARCHAR`. `SQLite` stores JSON
    /// columns as `TEXT` so no cast is needed. `PostgreSQL` stores them as `JSONB`,
    /// which requires an explicit cast from the bound `TEXT` value — otherwise the
    /// backend rejects the insert/update with "column is of type jsonb but expression
    /// is of type text". Append this suffix directly after the `?` placeholder for
    /// that bind position, e.g. `format!("VALUES (?{json_cast})", json_cast = ...)`.
    ///
    /// `SQLite`: empty string
    /// `PostgreSQL`: `::jsonb`
    const JSON_CAST: &'static str;

    /// Project a non-`TEXT` column so it decodes into a plain `String`.
    ///
    /// `SQLite` is dynamically typed and stores JSON/timestamp columns as `TEXT`, so
    /// the column already decodes into `String` directly — no cast needed. `PostgreSQL`
    /// stores the same logical data in natively-typed columns (`JSONB`, `TIMESTAMPTZ`,
    /// ...); decoding those straight into `String` fails (`sqlx`'s `String:
    /// Decode<Postgres>` only covers `TEXT`-family OIDs), so the column must be cast to
    /// `::text` in the `SELECT` list for call sites that only need the raw text
    /// representation (rather than a typed decode via `sqlx::types::Json<T>` or
    /// `chrono::DateTime`).
    ///
    /// `SQLite`: `{col}`
    /// `PostgreSQL`: `{col}::text`
    fn select_as_text(col: &str) -> String;

    /// Scalar function name returning the greatest of two (or more) numeric arguments.
    ///
    /// `SQLite`'s `max(a, b, ...)` is a scalar multi-argument function. `PostgreSQL`'s
    /// `MAX()` is exclusively an aggregate (requires `GROUP BY`) — the scalar equivalent
    /// is `GREATEST(a, b, ...)`. Use this constant when building a two-argument "largest
    /// of" expression that must work as a plain scalar call, not an aggregate.
    ///
    /// `SQLite`: `MAX`
    /// `PostgreSQL`: `GREATEST`
    const GREATEST_FN: &'static str;

    /// Scalar function name returning the least of two (or more) numeric arguments.
    ///
    /// `SQLite`'s `min(a, b, ...)` is a scalar multi-argument function. `PostgreSQL`'s
    /// `MIN()` is exclusively an aggregate (requires `GROUP BY`) — the scalar equivalent
    /// is `LEAST(a, b, ...)`. Use this constant when building a two-argument "smallest
    /// of" expression that must work as a plain scalar call, not an aggregate.
    ///
    /// `SQLite`: `MIN`
    /// `PostgreSQL`: `LEAST`
    const LEAST_FN: &'static str;
}

/// `SQLite` dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";
    const COLLATE_NOCASE: &'static str = "COLLATE NOCASE";
    const EPOCH_NOW: &'static str = "unixepoch('now')";
    const JSON_CAST: &'static str = "";
    const GREATEST_FN: &'static str = "MAX";
    const LEAST_FN: &'static str = "MIN";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(strftime('%s', {col}) AS INTEGER), 0)")
    }

    fn select_as_text(col: &str) -> String {
        col.to_string()
    }
}

/// `PostgreSQL` dialect marker type.
pub struct Postgres;

impl Dialect for Postgres {
    const AUTO_PK: &'static str = "BIGSERIAL PRIMARY KEY";
    const INSERT_IGNORE: &'static str = "INSERT";
    const CONFLICT_NOTHING: &'static str = "ON CONFLICT DO NOTHING";
    const COLLATE_NOCASE: &'static str = "";
    const EPOCH_NOW: &'static str = "EXTRACT(EPOCH FROM NOW())::BIGINT";
    const JSON_CAST: &'static str = "::jsonb";
    const GREATEST_FN: &'static str = "GREATEST";
    const LEAST_FN: &'static str = "LEAST";

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(EXTRACT(EPOCH FROM {col}) AS BIGINT), 0)")
    }

    fn select_as_text(col: &str) -> String {
        format!("{col}::text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;

        #[test]
        fn auto_pk() {
            assert_eq!(Sqlite::AUTO_PK, "INTEGER PRIMARY KEY AUTOINCREMENT");
        }

        #[test]
        fn insert_ignore() {
            assert_eq!(Sqlite::INSERT_IGNORE, "INSERT OR IGNORE");
            assert_eq!(Sqlite::CONFLICT_NOTHING, "");
        }

        #[test]
        fn epoch_now() {
            assert_eq!(Sqlite::EPOCH_NOW, "unixepoch('now')");
        }

        #[test]
        fn epoch_from_col() {
            assert_eq!(
                Sqlite::epoch_from_col("created_at"),
                "COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0)"
            );
        }

        #[test]
        fn select_as_text() {
            assert_eq!(Sqlite::select_as_text("parts"), "parts");
        }

        #[test]
        fn ilike() {
            assert_eq!(Sqlite::ilike("name"), "name COLLATE NOCASE");
        }

        #[test]
        fn json_cast() {
            assert_eq!(Sqlite::JSON_CAST, "");
        }

        #[test]
        fn greatest_fn() {
            assert_eq!(Sqlite::GREATEST_FN, "MAX");
        }

        #[test]
        fn least_fn() {
            assert_eq!(Sqlite::LEAST_FN, "MIN");
        }
    }

    #[cfg(feature = "postgres")]
    mod postgres {
        use super::*;

        #[test]
        fn auto_pk() {
            assert_eq!(Postgres::AUTO_PK, "BIGSERIAL PRIMARY KEY");
        }

        #[test]
        fn insert_ignore() {
            assert_eq!(Postgres::INSERT_IGNORE, "INSERT");
            assert_eq!(Postgres::CONFLICT_NOTHING, "ON CONFLICT DO NOTHING");
        }

        #[test]
        fn epoch_now() {
            assert_eq!(Postgres::EPOCH_NOW, "EXTRACT(EPOCH FROM NOW())::BIGINT");
        }

        #[test]
        fn epoch_from_col() {
            assert_eq!(
                Postgres::epoch_from_col("created_at"),
                "COALESCE(CAST(EXTRACT(EPOCH FROM created_at) AS BIGINT), 0)"
            );
        }

        #[test]
        fn select_as_text() {
            assert_eq!(Postgres::select_as_text("parts"), "parts::text");
        }

        #[test]
        fn ilike() {
            assert_eq!(Postgres::ilike("name"), "LOWER(name)");
        }

        #[test]
        fn json_cast() {
            assert_eq!(Postgres::JSON_CAST, "::jsonb");
        }

        #[test]
        fn greatest_fn() {
            assert_eq!(Postgres::GREATEST_FN, "GREATEST");
        }

        #[test]
        fn least_fn() {
            assert_eq!(Postgres::LEAST_FN, "LEAST");
        }
    }
}
