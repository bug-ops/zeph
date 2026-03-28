// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// SQL fragments that differ between database backends.
///
/// Implemented by zero-sized marker types ([`Sqlite`], [`Postgres`]).
/// All associated constants are `&'static str` for zero-cost usage.
pub trait Dialect: Send + Sync + 'static {
    /// The `NOW()` expression for this backend.
    ///
    /// `SQLite`: `datetime('now')`
    /// `PostgreSQL`: `now()`
    const NOW: &'static str;

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

    /// Case-insensitive comparison expression for a column.
    ///
    /// `SQLite`: `{col} COLLATE NOCASE`
    /// `PostgreSQL`: `LOWER({col})`
    fn ilike(col: &str) -> String;
}

/// `SQLite` dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const NOW: &'static str = "datetime('now')";
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }
}

/// `PostgreSQL` dialect marker type.
pub struct Postgres;

impl Dialect for Postgres {
    const NOW: &'static str = "now()";
    const AUTO_PK: &'static str = "BIGSERIAL PRIMARY KEY";
    const INSERT_IGNORE: &'static str = "INSERT";
    const CONFLICT_NOTHING: &'static str = "ON CONFLICT DO NOTHING";

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }
}
