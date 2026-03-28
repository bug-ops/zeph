// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{DatabaseDriver, dialect::Postgres, error::DbError};

/// PostgreSQL backend driver.
pub struct PostgresDriver;

impl DatabaseDriver for PostgresDriver {
    type Database = sqlx::Postgres;
    type Dialect = Postgres;

    async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), DbError> {
        sqlx::migrate!("./migrations/postgres")
            .run(pool)
            .await
            .map_err(DbError::Migration)?;
        Ok(())
    }

    async fn begin(
        pool: &sqlx::PgPool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
        pool.begin().await
    }

    async fn begin_write(
        pool: &sqlx::PgPool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
        // PostgreSQL uses MVCC; standard BEGIN is sufficient.
        // For write-exclusion semantics, callers must use SELECT ... FOR UPDATE
        // inside the transaction.
        pool.begin().await
    }
}
