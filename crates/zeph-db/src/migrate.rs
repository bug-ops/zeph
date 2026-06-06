// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// Both items are only referenced by the backend-gated `run_migrations` definitions below;
// gate the import so a no-backend build surfaces only the crate-level `compile_error!`
// rather than a spurious unused-import warning (#4956).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
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
#[tracing::instrument(name = "db.migrate.run", skip_all, err)]
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
#[tracing::instrument(name = "db.migrate.run", skip_all, err)]
pub async fn run_migrations(pool: &DbPool) -> Result<(), DbError> {
    sqlx::migrate!("./migrations/postgres")
        .run(pool)
        .await
        .map_err(DbError::Migration)?;
    Ok(())
}
