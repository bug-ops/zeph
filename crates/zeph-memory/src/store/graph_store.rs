// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw graph persistence trait and [`TaskGraphStore`] implementation.
//!
//! The trait ([`zeph_db::RawGraphStore`]) operates on opaque JSON strings to avoid a
//! dependency cycle (`zeph-core` → `zeph-memory` → `zeph-core`). `zeph-core` wraps
//! this trait in `GraphPersistence<S>` which handles typed serialization.

#[allow(unused_imports)]
use zeph_db::sql;
use zeph_db::{DbError, DbPool};

// Re-export for callers that previously imported from this module.
pub use zeph_db::{GraphSummary, RawGraphStore};

/// Database-backed implementation of [`RawGraphStore`].
#[derive(Debug, Clone)]
pub struct TaskGraphStore {
    pool: DbPool,
}

impl TaskGraphStore {
    /// Create a new [`TaskGraphStore`] backed by the given pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

impl RawGraphStore for TaskGraphStore {
    #[tracing::instrument(skip_all, name = "memory.graph.save_graph")]
    async fn save_graph(
        &self,
        id: &str,
        goal: &str,
        status: &str,
        graph_json: &str,
        created_at: &str,
        finished_at: Option<&str>,
    ) -> Result<(), DbError> {
        zeph_db::query(sql!(
            "INSERT INTO task_graphs (id, goal, status, graph_json, created_at, finished_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                 goal        = excluded.goal, \
                 status      = excluded.status, \
                 graph_json  = excluded.graph_json, \
                 created_at  = excluded.created_at, \
                 finished_at = excluded.finished_at"
        ))
        .bind(id)
        .bind(goal)
        .bind(status)
        .bind(graph_json)
        .bind(created_at)
        .bind(finished_at)
        .execute(&self.pool)
        .await
        .map_err(DbError::Sqlx)?;
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[tracing::instrument(skip_all, name = "memory.graph.load_graph")]
    async fn load_graph(&self, id: &str) -> Result<Option<String>, DbError> {
        let row: Option<(String,)> =
            zeph_db::query_as(sql!("SELECT graph_json FROM task_graphs WHERE id = ?"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(DbError::Sqlx)?;
        Ok(row.map(|(json,)| json))
    }

    #[cfg(feature = "sqlite")]
    async fn list_graphs(&self, limit: u32) -> Result<Vec<GraphSummary>, DbError> {
        let rows: Vec<(String, String, String, String, Option<String>)> = zeph_db::query_as(sql!(
            "SELECT id, goal, status, created_at, finished_at \
             FROM task_graphs \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::Sqlx)?;

        Ok(rows
            .into_iter()
            .map(|(id, goal, status, created_at, finished_at)| GraphSummary {
                id,
                goal,
                status,
                created_at,
                finished_at,
            })
            .collect())
    }

    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.graph.load_graph")]
    async fn load_graph(&self, id: &str) -> Result<Option<String>, DbError> {
        let row: Option<String> = sqlx::query_scalar::<sqlx::Postgres, String>(sql!(
            "SELECT graph_json FROM task_graphs WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(DbError::Sqlx)?;
        Ok(row)
    }

    #[cfg(feature = "postgres")]
    async fn list_graphs(&self, limit: u32) -> Result<Vec<GraphSummary>, DbError> {
        use sqlx::Row as _;

        let rows = sqlx::query::<sqlx::Postgres>(sql!(
            "SELECT id, goal, status, created_at, finished_at \
             FROM task_graphs \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::Sqlx)?;

        Ok(rows
            .into_iter()
            .map(|row| GraphSummary {
                id: row.get("id"),
                goal: row.get("goal"),
                status: row.get("status"),
                created_at: row.get("created_at"),
                finished_at: row.get("finished_at"),
            })
            .collect())
    }

    async fn delete_graph(&self, id: &str) -> Result<bool, DbError> {
        let result = zeph_db::query(sql!("DELETE FROM task_graphs WHERE id = ?"))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(DbError::Sqlx)?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    async fn make_store() -> TaskGraphStore {
        let db = SqliteStore::new(":memory:").await.expect("SqliteStore");
        TaskGraphStore::new(db.pool().clone())
    }

    #[tokio::test]
    async fn test_save_and_load_roundtrip() {
        let store = make_store().await;
        store
            .save_graph("id-1", "goal", "created", r#"{"key":"val"}"#, "100", None)
            .await
            .expect("save");
        let loaded = store
            .load_graph("id-1")
            .await
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded, r#"{"key":"val"}"#);
    }

    #[tokio::test]
    async fn test_load_nonexistent() {
        let store = make_store().await;
        let result = store.load_graph("missing-id").await.expect("load");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_graphs_ordering() {
        let store = make_store().await;
        store
            .save_graph("id-1", "first", "created", "{}", "100", None)
            .await
            .expect("save 1");
        store
            .save_graph("id-2", "second", "created", "{}", "200", None)
            .await
            .expect("save 2");
        let list = store.list_graphs(10).await.expect("list");
        assert_eq!(list.len(), 2);
        // Ordered by created_at DESC: id-2 (200) before id-1 (100)
        assert_eq!(list[0].id, "id-2");
        assert_eq!(list[1].id, "id-1");
    }

    #[tokio::test]
    async fn test_delete_graph() {
        let store = make_store().await;
        store
            .save_graph("id-del", "goal", "created", "{}", "1", None)
            .await
            .expect("save");
        let deleted = store.delete_graph("id-del").await.expect("delete");
        assert!(deleted);
        let loaded = store.load_graph("id-del").await.expect("load");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_save_overwrites_existing() {
        let store = make_store().await;
        store
            .save_graph("id-1", "old", "created", r#"{"v":1}"#, "1", None)
            .await
            .expect("save 1");
        store
            .save_graph("id-1", "new", "running", r#"{"v":2}"#, "1", None)
            .await
            .expect("save 2 (upsert)");
        let loaded = store
            .load_graph("id-1")
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(loaded, r#"{"v":2}"#);
    }
}
