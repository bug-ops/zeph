// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::DbPool;

/// Begin a standard deferred transaction.
///
/// # Errors
///
/// Returns a sqlx error if the transaction cannot be started.
pub async fn begin(pool: &DbPool) -> Result<crate::DbTransaction<'_>, sqlx::Error> {
    pool.begin().await
}

/// Begin a write-intent transaction.
///
/// `SQLite`: issues `BEGIN IMMEDIATE` to acquire the write lock upfront,
/// preventing `SQLITE_BUSY` errors when another writer is active.
///
/// `PostgreSQL`: issues a standard `BEGIN` (MVCC handles concurrency).
/// Callers that need write-exclusion semantics must use `SELECT ... FOR UPDATE`
/// inside the transaction.
///
/// # Errors
///
/// Returns a sqlx error if the transaction cannot be started.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn begin_write(pool: &DbPool) -> Result<crate::DbTransaction<'_>, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Begin a read-write transaction on `PostgreSQL`.
///
/// # Errors
///
/// Returns a sqlx error if the transaction cannot be started.
#[cfg(feature = "postgres")]
pub async fn begin_write(pool: &DbPool) -> Result<crate::DbTransaction<'_>, sqlx::Error> {
    pool.begin().await
}
