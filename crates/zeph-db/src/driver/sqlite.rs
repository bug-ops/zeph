// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{DatabaseDriver, dialect::Sqlite};

/// `SQLite` backend driver.
pub struct SqliteDriver;

impl DatabaseDriver for SqliteDriver {
    type Database = sqlx::Sqlite;
    type Dialect = Sqlite;
}
