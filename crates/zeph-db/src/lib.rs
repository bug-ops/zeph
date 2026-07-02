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
//! default includes `zeph-db/sqlite`. When both are enabled simultaneously,
//! `postgres` takes priority. Use `--features full` for the standard `SQLite` build.

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("exactly one of `sqlite` or `postgres` must be enabled for `zeph-db`");

pub mod bounds;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod fts;
pub mod graph_store;
pub mod migrate;
pub mod pool;
pub mod transaction;

pub use bounds::FullDriver;
pub use dialect::{Dialect, Postgres, Sqlite};
pub use driver::DatabaseDriver;
pub use error::DbError;
pub use graph_store::{GraphSummary, RawGraphStore};
pub use migrate::run_migrations;
pub use pool::{DbConfig, redact_url};
pub use transaction::{begin, begin_write};

// Re-export sqlx query builders bound to the active backend.
pub use sqlx::query_builder::QueryBuilder;
pub use sqlx::{Error as SqlxError, Executor, FromRow, Row, query, query_as, query_scalar};
// Re-export the full sqlx crate so consumers can use `zeph_db::sqlx::Type` etc.
pub use sqlx;

// --- Active driver type alias ---

/// The active database driver, selected at compile time.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type ActiveDriver = driver::SqliteDriver;
#[cfg(feature = "postgres")]
pub type ActiveDriver = driver::PostgresDriver;

// --- Convenience type aliases ---

/// A connection pool for the active database backend.
///
/// Resolves to `sqlx::SqlitePool` or `sqlx::PgPool` at compile time depending on the active driver.
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
/// `PostgreSQL`: rewrites `?` to `$1`, `$2`, ... and `?N` (`SQLite`'s numbered
/// placeholder, e.g. `?1`) to `$N` using [`rewrite_placeholders`]. Repeated
/// references to the same `?N` collapse to the same `$N`. The rewritten
/// string is leaked via `Box::leak` to obtain `&'static str` —
/// no caching: each call site leaks one allocation per unique SQL string.
/// The set of unique SQL strings is bounded (call sites are fixed at compile
/// time), so total leaked memory is bounded and acceptable for a long-running
/// process. Do NOT wrap `PostgreSQL` JSONB queries using `?`/`?|`/`?&`
/// operators through this macro; use `$N` placeholders directly for those.
///
/// # Example
///
/// ```rust,ignore
/// let rows = sqlx::query(sql!("SELECT id FROM messages WHERE conversation_id = ?"))
///     .bind(cid)
///     .fetch_all(&pool)
///     .await?;
/// ```
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
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
/// Check whether `url` looks like a `PostgreSQL` connection URL.
///
/// Used to detect misconfigured `database_url` values (e.g. a `SQLite` path passed
/// to a postgres build, or vice versa).
#[must_use]
pub fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Rewrite `?` bind markers to `$1, $2, ...` for `PostgreSQL`.
///
/// Handles both bare `?` (`SQLite`'s "next unused index" placeholder) and
/// numbered `?N` (`SQLite`'s explicit-index placeholder, e.g. `?1`, `?2`).
/// A numbered `?N` is rewritten directly to `$N` — preserving the index
/// rather than renumbering by position — so that repeated references to the
/// same `?N` collapse to the same `$N` and require only one `.bind()` call,
/// matching both `SQLite`'s and `sqlx`'s `PostgreSQL` binding semantics (bind
/// values are supplied once per distinct slot, in ascending slot order,
/// regardless of how many times the slot's placeholder appears in the SQL
/// text). A bare `?` is assigned the next index after the highest index used
/// so far (explicit or bare), matching `SQLite`'s own numbering rule.
///
/// Skips `?` inside single-quoted string literals. Does NOT handle dollar-quoted
/// strings (`$$...$$`) or `?` inside comments — document this limitation at call
/// sites where those patterns appear.
#[must_use]
pub fn rewrite_placeholders(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 16);
    let mut chars = query.chars().peekable();
    let mut max_n = 0u32;
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                in_string = !in_string;
                out.push(ch);
            }
            '?' if !in_string => {
                let digits: String =
                    std::iter::from_fn(|| chars.next_if(char::is_ascii_digit)).collect();
                let n = if digits.is_empty() {
                    max_n += 1;
                    max_n
                } else {
                    let explicit = digits.parse().unwrap_or(u32::MAX);
                    max_n = max_n.max(explicit);
                    explicit
                };
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Generate a single numbered placeholder for bind position `n` (1-based).
///
/// `SQLite`: `?N`, `PostgreSQL`: `$N`
#[must_use]
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn numbered_placeholder(n: usize) -> String {
    format!("?{n}")
}

/// Generate a single numbered placeholder for bind position `n` (1-based).
///
/// `SQLite`: `?N`, `PostgreSQL`: `$N`
#[must_use]
#[cfg(feature = "postgres")]
pub fn numbered_placeholder(n: usize) -> String {
    format!("${n}")
}

/// Generate a comma-separated list of placeholders for `count` binds starting at position
/// `start` (1-based).
///
/// Example (SQLite): `placeholder_list(3, 2)` → `"?3, ?4"`
/// Example (PostgreSQL): `placeholder_list(3, 2)` → `"$3, $4"`
#[must_use]
pub fn placeholder_list(start: usize, count: usize) -> String {
    (start..start + count)
        .map(numbered_placeholder)
        .collect::<Vec<_>>()
        .join(", ")
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

    #[test]
    fn rewrite_placeholders_numbered() {
        let out = rewrite_placeholders("INSERT INTO t (a, b) VALUES (?1, ?2) RETURNING id");
        assert_eq!(out, "INSERT INTO t (a, b) VALUES ($1, $2) RETURNING id");
    }

    #[test]
    fn rewrite_placeholders_collapses_repeated_numbered() {
        // `?1` appears twice — SQLite binds it once, so PostgreSQL must too.
        let out = rewrite_placeholders("INSERT INTO edges (source_id, target_id) VALUES (?1, ?1)");
        assert_eq!(
            out,
            "INSERT INTO edges (source_id, target_id) VALUES ($1, $1)"
        );
    }

    #[test]
    fn rewrite_placeholders_mixed_bare_and_numbered() {
        // A bare `?` after `?2` continues numbering from the highest index seen (3),
        // matching SQLite's "next unused index" rule for anonymous placeholders.
        let out = rewrite_placeholders("SELECT * FROM t WHERE a = ?2 AND b = ? AND c = ?1");
        assert_eq!(out, "SELECT * FROM t WHERE a = $2 AND b = $3 AND c = $1");
    }

    #[test]
    fn numbered_placeholder_one_based() {
        let p1 = numbered_placeholder(1);
        let p3 = numbered_placeholder(3);
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        {
            assert_eq!(p1, "?1");
            assert_eq!(p3, "?3");
        }
        #[cfg(feature = "postgres")]
        {
            assert_eq!(p1, "$1");
            assert_eq!(p3, "$3");
        }
    }

    #[test]
    fn placeholder_list_range() {
        let list = placeholder_list(2, 3);
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        assert_eq!(list, "?2, ?3, ?4");
        #[cfg(feature = "postgres")]
        assert_eq!(list, "$2, $3, $4");
    }

    #[test]
    fn placeholder_list_empty() {
        assert_eq!(placeholder_list(1, 0), "");
    }
}
