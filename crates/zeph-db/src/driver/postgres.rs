// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{DatabaseDriver, dialect::Postgres};

/// PostgreSQL backend driver.
pub struct PostgresDriver;

impl DatabaseDriver for PostgresDriver {
    type Database = sqlx::Postgres;
    type Dialect = Postgres;
}
