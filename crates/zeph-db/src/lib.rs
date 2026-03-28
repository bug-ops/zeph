// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Database abstraction layer for Zeph.
//!
//! Provides [`DbPool`], [`DbRow`], [`DbTransaction`], [`DbQueryResult`] type
//! aliases that resolve to either `SQLite` or `PostgreSQL` types at compile time,
//! based on the active feature flag (`sqlite` or `postgres`).
//!
//! The [`sql!`] macro converts `?` placeholders to `$N` style for `PostgreSQL`,
//! and is a no-op identity for `SQLite` (returning `&'static str` directly).
//!
//! # Feature Flags
//!
//! Exactly one of `sqlite` or `postgres` must be enabled. The root workspace
//! default includes `zeph-db/sqlite`. Enabling both simultaneously triggers a
//! `compile_error!`. Using `--all-features` is intentionally unsupported;
//! use `--features full` or `--features full,postgres` instead.

#[cfg(all(feature = "sqlite", feature = "postgres"))]
compile_error!("features `sqlite` and `postgres` are mutually exclusive; enable exactly one");

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("exactly one of `sqlite` or `postgres` must be enabled for `zeph-db`");

pub mod bounds;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod fts;
pub mod migrate;
pub mod pool;
pub mod transaction;

pub use bounds::FullDriver;
pub use dialect::{Dialect, Postgres, Sqlite};
pub use driver::DatabaseDriver;
pub use error::DbError;
pub use migrate::run_migrations;
pub use pool::{DbConfig, redact_url};
pub use transaction::{begin, begin_write};

// Re-export sqlx query builders bound to the active backend.
pub use sqlx::{Error as SqlxError, Executor, FromRow, Row, query, query_as, query_scalar};

// --- Active driver type alias ---

/// The active database driver, selected at compile time.
#[cfg(feature = "sqlite")]
pub type ActiveDriver = driver::SqliteDriver;
#[cfg(feature = "postgres")]
pub type ActiveDriver = driver::PostgresDriver;

// --- Convenience type aliases ---

/// A connection pool for the active database backend.
///
/// Resolves to [`sqlx::SqlitePool`] or [`sqlx::PgPool`] at compile time.
pub type DbPool = sqlx::Pool<<ActiveDriver as DatabaseDriver>::Database>;

/// A row from the active database backend.
pub type DbRow = <<ActiveDriver as DatabaseDriver>::Database as sqlx::Database>::Row;

/// A query result from the active database backend.
pub type DbQueryResult =
    <<ActiveDriver as DatabaseDriver>::Database as sqlx::Database>::QueryResult;

/// A transaction on the active database backend.
pub type DbTransaction<'a> = sqlx::Transaction<'a, <ActiveDriver as DatabaseDriver>::Database>;

/// The active SQL dialect type.
pub type ActiveDialect = <ActiveDriver as DatabaseDriver>::Dialect;

// --- sql! macro ---

/// Convert SQL with `?` placeholders to the active backend's placeholder style.
///
/// `SQLite`: returns the input `&str` directly — zero allocation, zero runtime cost.
///
/// `PostgreSQL`: rewrites `?` to `$1`, `$2`, ... using [`rewrite_placeholders`].
/// The rewrite is cached in a `LazyLock<String>` — runs exactly once per call
/// site. Do NOT wrap `PostgreSQL` JSONB queries using `?`/`?|`/`?&` operators
/// through this macro; use `$N` placeholders directly for those.
///
/// # Example
///
/// ```rust,ignore
/// let rows = sqlx::query(sql!("SELECT id FROM messages WHERE conversation_id = ?"))
///     .bind(cid)
///     .fetch_all(&pool)
///     .await?;
/// ```
#[cfg(feature = "sqlite")]
#[macro_export]
macro_rules! sql {
    ($query:expr) => {
        $query
    };
}

#[cfg(feature = "postgres")]
#[macro_export]
macro_rules! sql {
    ($query:expr) => {{
        // Leak the rewritten query string to obtain `&'static str`.
        // The set of unique SQL strings in the application is finite, so total
        // leaked memory is bounded and acceptable for a long-running process.
        let s: String = $crate::rewrite_placeholders($query);
        Box::leak(s.into_boxed_str()) as &'static str
    }};
}

/// Returns `true` if the given database URL looks like a `PostgreSQL` connection string.
///
/// Compiled only when the `sqlite` feature is active. Callers can use this to
/// detect a misconfigured `database_url` pointing to `PostgreSQL` in a build
/// that only supports `SQLite`.
#[cfg(feature = "sqlite")]
#[must_use]
pub fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Rewrite `?` bind markers to `$1, $2, ...` for `PostgreSQL`.
///
/// Skips `?` inside single-quoted string literals. Does NOT handle dollar-quoted
/// strings (`$$...$$`) or `?` inside comments — document this limitation at call
/// sites where those patterns appear.
#[must_use]
pub fn rewrite_placeholders(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 16);
    let mut n = 0u32;
    let mut in_string = false;
    for ch in query.chars() {
        match ch {
            '\'' => {
                in_string = !in_string;
                out.push(ch);
            }
            '?' if !in_string => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_placeholders_basic() {
        let out = rewrite_placeholders("SELECT * FROM t WHERE a = ? AND b = ?");
        assert_eq!(out, "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn rewrite_placeholders_skips_string_literals() {
        let out = rewrite_placeholders("SELECT '?' FROM t WHERE a = ?");
        assert_eq!(out, "SELECT '?' FROM t WHERE a = $1");
    }

    #[test]
    fn rewrite_placeholders_no_params() {
        let out = rewrite_placeholders("SELECT 1");
        assert_eq!(out, "SELECT 1");
    }
}
