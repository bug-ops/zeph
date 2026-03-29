// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw graph persistence trait and [`DbGraphStore`] implementation.
//!
//! The trait operates on opaque JSON strings to avoid a dependency cycle
//! (`zeph-core` → `zeph-memory` → `zeph-core`). `zeph-core` wraps this
//! trait in `GraphPersistence<S>` which handles typed serialization.

use zeph_db::DbPool;
#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::MemoryError;

/// Summary of a stored task graph (metadata only, no task details).
#[derive(Debug, Clone)]
pub struct GraphSummary {
    pub id: String,
    pub goal: String,
    pub status: String,
    pub created_at: String,
    pub finished_at: Option<String>,
}

/// Raw persistence interface for task graphs.
///
/// All graph data is stored as a JSON blob. The orchestration layer in
/// `zeph-core` is responsible for serializing/deserializing `TaskGraph`.
pub trait RawGraphStore: Send + Sync {
    /// Persist a graph (upsert by `id`).
    ///
    /// # Errors
    ///
    /// Returns a `MemoryError` on database failure.
    #[allow(async_fn_in_trait)]
    async fn save_graph(
        &self,
        id: &str,
        goal: &str,
        status: &str,
        graph_json: &str,
        created_at: &str,
        finished_at: Option<&str>,
    ) -> Result<(), MemoryError>;

    /// Load a graph by its string UUID.
    ///
    /// Returns `None` if the graph does not exist.
    ///
    /// # Errors
    ///
    /// Returns a `MemoryError` on database failure.
    #[allow(async_fn_in_trait)]
    async fn load_graph(&self, id: &str) -> Result<Option<String>, MemoryError>;

    /// List graphs ordered by `created_at` descending, limited to `limit` rows.
    ///
    /// # Errors
    ///
    /// Returns a `MemoryError` on database failure.
    #[allow(async_fn_in_trait)]
    async fn list_graphs(&self, limit: u32) -> Result<Vec<GraphSummary>, MemoryError>;

    /// Delete a graph by its string UUID.
    ///
    /// Returns `true` if a row was deleted.
    ///
    /// # Errors
    ///
    /// Returns a `MemoryError` on database failure.
    #[allow(async_fn_in_trait)]
    async fn delete_graph(&self, id: &str) -> Result<bool, MemoryError>;
}

/// Database-backed implementation of [`RawGraphStore`].
#[derive(Debug, Clone)]
pub struct DbGraphStore {
    pool: DbPool,
}

/// Backward-compatible alias.
pub type SqliteGraphStore = DbGraphStore;

impl DbGraphStore {
    /// Create a new [`DbGraphStore`] backed by the given pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

impl RawGraphStore for DbGraphStore {
    async fn save_graph(
        &self,
        id: &str,
        goal: &str,
        status: &str,
        graph_json: &str,
        created_at: &str,
        finished_at: Option<&str>,
    ) -> Result<(), MemoryError> {
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
        .map_err(|e| MemoryError::GraphStore(e.to_string()))?;
        Ok(())
    }

    async fn load_graph(&self, id: &str) -> Result<Option<String>, MemoryError> {
        let row: Option<(String,)> =
            zeph_db::query_as(sql!("SELECT graph_json FROM task_graphs WHERE id = ?"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| MemoryError::GraphStore(e.to_string()))?;
        Ok(row.map(|(json,)| json))
    }

    async fn list_graphs(&self, limit: u32) -> Result<Vec<GraphSummary>, MemoryError> {
        let rows: Vec<(String, String, String, String, Option<String>)> = zeph_db::query_as(sql!(
            "SELECT id, goal, status, created_at, finished_at \
             FROM task_graphs \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::GraphStore(e.to_string()))?;

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

    async fn delete_graph(&self, id: &str) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!("DELETE FROM task_graphs WHERE id = ?"))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::GraphStore(e.to_string()))?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DbStore;

    async fn make_store() -> DbGraphStore {
        let db = DbStore::new(":memory:").await.expect("DbStore");
        DbGraphStore::new(db.pool().clone())
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
