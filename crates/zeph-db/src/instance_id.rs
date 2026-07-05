// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stable per-database instance identity (#5742).
//!
//! Autoincrementing primary keys (e.g. `conversation_id`) are unique only within one
//! physical database. When multiple independent databases share a single external
//! store (e.g. Qdrant), those keys collide. [`get_or_create_instance_id`] gives each
//! database a random UUID, generated once and persisted, that callers combine with
//! such keys to disambiguate them across databases.

use crate::{DbPool, error::DbError, sql};

/// Fetch this database's stable instance UUID, generating and persisting one on first call.
///
/// Race-safe: the insert is `ON CONFLICT DO NOTHING`, so the first writer's row wins and
/// every caller — including concurrent ones racing on a fresh database — reads back the
/// same persisted value afterward.
///
/// # Errors
///
/// Returns [`DbError`] if the insert or the follow-up select fails.
///
/// # Examples
///
/// ```rust,no_run
/// # async fn example(pool: &zeph_db::DbPool) -> Result<(), zeph_db::DbError> {
/// let instance_id = zeph_db::get_or_create_instance_id(pool).await?;
/// # let _ = instance_id;
/// # Ok(())
/// # }
/// ```
pub async fn get_or_create_instance_id(pool: &DbPool) -> Result<String, DbError> {
    let candidate = uuid::Uuid::new_v4().to_string();
    sqlx::query(sql!(
        "INSERT INTO db_instance (id, instance_id) VALUES (1, ?) ON CONFLICT (id) DO NOTHING"
    ))
    .bind(&candidate)
    .execute(pool)
    .await?;
    sqlx::query_scalar(sql!("SELECT instance_id FROM db_instance WHERE id = 1"))
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbConfig, run_migrations};

    async fn setup_pool() -> DbPool {
        let pool = DbConfig {
            url: ":memory:".to_string(),
            max_connections: 1,
            pool_size: 1,
        }
        .connect()
        .await
        .expect("connect");
        run_migrations(&pool).await.expect("migrate");
        pool
    }

    #[tokio::test]
    async fn generates_and_persists_id() {
        let pool = setup_pool().await;
        let id1 = get_or_create_instance_id(&pool).await.expect("first call");
        assert!(!id1.is_empty());
        let id2 = get_or_create_instance_id(&pool).await.expect("second call");
        assert_eq!(id1, id2, "instance id must be stable across calls");
    }

    #[tokio::test]
    async fn distinct_pools_get_distinct_ids() {
        let pool_a = setup_pool().await;
        let pool_b = setup_pool().await;
        let id_a = get_or_create_instance_id(&pool_a).await.expect("pool a");
        let id_b = get_or_create_instance_id(&pool_b).await.expect("pool b");
        assert_ne!(id_a, id_b, "distinct databases must get distinct ids");
    }
}
