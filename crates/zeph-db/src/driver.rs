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
/// Connection logic lives exclusively in [`crate::DbConfig`] to avoid duplication.
/// This trait covers only the operations that are truly backend-specific at runtime:
/// migrations, and transaction semantics.
pub trait DatabaseDriver: Send + Sync + 'static {
    /// The sqlx `Database` type (e.g., `sqlx::Sqlite`, `sqlx::Postgres`).
    type Database: sqlx::Database;

    /// The dialect providing SQL fragment constants.
    type Dialect: crate::dialect::Dialect;

    /// Run all pending migrations.
    ///
    /// # Errors
    ///
    /// Returns [`crate::DbError`] if any migration fails.
    fn run_migrations(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<Output = Result<(), crate::error::DbError>> + Send;

    /// Begin a standard deferred transaction.
    ///
    /// # Errors
    ///
    /// Returns a sqlx error if the transaction cannot be started.
    fn begin(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<
        Output = Result<sqlx::Transaction<'_, Self::Database>, sqlx::Error>,
    > + Send;

    /// Begin a write-intent transaction.
    ///
    /// `SQLite`: issues `BEGIN IMMEDIATE` to acquire the write lock upfront.
    /// `PostgreSQL`: issues a standard `BEGIN` (MVCC handles concurrency).
    ///
    /// # Errors
    ///
    /// Returns a sqlx error if the transaction cannot be started.
    fn begin_write(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<
        Output = Result<sqlx::Transaction<'_, Self::Database>, sqlx::Error>,
    > + Send;
}
