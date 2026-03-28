// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`FullDriver`] blanket super-trait for reducing sqlx bound repetition.

use crate::DatabaseDriver;

/// Marker trait automatically implemented for all [`DatabaseDriver`] types
/// whose `Database` supports standard Rust types in queries (sqlx 0.8 bounds).
///
/// This trait exists solely to reduce bound repetition on generic impl blocks.
/// It is sealed: all impls are inside this crate.
pub trait FullDriver: DatabaseDriver
where
    for<'q> <Self::Database as sqlx::Database>::Arguments<'q>:
        sqlx::IntoArguments<'q, Self::Database>,
    for<'c> &'c mut <Self::Database as sqlx::Database>::Connection:
        sqlx::Executor<'c, Database = Self::Database>,
    i64: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    i32: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    String: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    bool: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    Vec<u8>: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
{
}

#[cfg(feature = "sqlite")]
impl FullDriver for crate::driver::SqliteDriver {}
#[cfg(feature = "postgres")]
impl FullDriver for crate::driver::PostgresDriver {}
