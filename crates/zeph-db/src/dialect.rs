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
    ///
    /// TODO Phase 2: `PostgreSQL` callers should switch to `ILIKE` predicates.
    const COLLATE_NOCASE: &'static str;

    /// Case-insensitive comparison expression for a column.
    ///
    /// `SQLite`: `{col} COLLATE NOCASE`
    /// `PostgreSQL`: `LOWER({col})`
    fn ilike(col: &str) -> String;
}

/// `SQLite` dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";
    const COLLATE_NOCASE: &'static str = "COLLATE NOCASE";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }
}

/// `PostgreSQL` dialect marker type.
pub struct Postgres;

impl Dialect for Postgres {
    const AUTO_PK: &'static str = "BIGSERIAL PRIMARY KEY";
    const INSERT_IGNORE: &'static str = "INSERT";
    const CONFLICT_NOTHING: &'static str = "ON CONFLICT DO NOTHING";
    const COLLATE_NOCASE: &'static str = "";

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }
}
