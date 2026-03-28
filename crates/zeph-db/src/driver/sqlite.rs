// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{DatabaseDriver, dialect::Sqlite, error::DbError};

/// `SQLite` backend driver.
pub struct SqliteDriver;

impl DatabaseDriver for SqliteDriver {
    type Database = sqlx::Sqlite;
    type Dialect = Sqlite;

    async fn run_migrations(pool: &sqlx::SqlitePool) -> Result<(), DbError> {
        sqlx::migrate!("./migrations/sqlite")
            .run(pool)
            .await
            .map_err(DbError::Migration)?;
        Ok(())
    }

    async fn begin(
        pool: &sqlx::SqlitePool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, sqlx::Error> {
        pool.begin().await
    }

    async fn begin_write(
        pool: &sqlx::SqlitePool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, sqlx::Error> {
        pool.begin_with("BEGIN IMMEDIATE").await
    }
}
