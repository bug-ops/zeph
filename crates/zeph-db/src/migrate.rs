// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{DbPool, error::DbError};

/// Run all pending migrations for the active backend.
///
/// `SQLite`: runs `migrations/sqlite/` directory.
/// `PostgreSQL`: runs `migrations/postgres/` directory.
///
/// # Errors
///
/// Returns [`DbError::Migration`] if any migration fails.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn run_migrations(pool: &DbPool) -> Result<(), DbError> {
    sqlx::migrate!("./migrations/sqlite")
        .run(pool)
        .await
        .map_err(DbError::Migration)?;
    Ok(())
}

/// Run all pending migrations for the `PostgreSQL` backend.
///
/// # Errors
///
/// Returns [`DbError::Migration`] if any migration fails.
#[cfg(feature = "postgres")]
pub async fn run_migrations(pool: &DbPool) -> Result<(), DbError> {
    sqlx::migrate!("./migrations/postgres")
        .run(pool)
        .await
        .map_err(DbError::Migration)?;
    Ok(())
}
