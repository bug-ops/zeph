// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`DatabaseDriver`] trait and per-backend implementations.

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::PostgresDriver;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteDriver;

/// Unifies a sqlx `Database` type with its [`crate::Dialect`].
///
/// Each backend (`SqliteDriver`, `PostgresDriver`) implements this trait once.
/// Consumer crates use `D: DatabaseDriver` as their single generic parameter,
/// which gives access to both `D::Database` (for sqlx pool/query bounds) and
/// `D::Dialect` (for SQL fragment substitution).
///
/// Connection, migration, and transaction logic live in [`crate::DbConfig`],
/// [`crate::migrate`], and [`crate::transaction`] respectively — not here.
pub trait DatabaseDriver: Send + Sync + 'static {
    /// The sqlx `Database` type (e.g., `sqlx::Sqlite`, `sqlx::Postgres`).
    type Database: sqlx::Database;

    /// The dialect providing SQL fragment constants.
    type Dialect: crate::dialect::Dialect;
}
