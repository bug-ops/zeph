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
}

/// `SQLite` dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";
    const COLLATE_NOCASE: &'static str = "COLLATE NOCASE";
    const EPOCH_NOW: &'static str = "unixepoch('now')";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(strftime('%s', {col}) AS INTEGER), 0)")
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

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(EXTRACT(EPOCH FROM {col}) AS BIGINT), 0)")
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
        fn ilike() {
            assert_eq!(Sqlite::ilike("name"), "name COLLATE NOCASE");
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
        fn ilike() {
            assert_eq!(Postgres::ilike("name"), "LOWER(name)");
        }
    }
}
