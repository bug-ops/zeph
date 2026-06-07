// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw graph persistence trait used by the orchestration layer.
//!
//! This module contains only the interface (`RawGraphStore`) and metadata types
//! (`GraphSummary`). The concrete SQLite/PostgreSQL implementation lives in
//! `zeph-memory::store::graph_store::TaskGraphStore`.

use crate::error::DbError;

/// Summary of a stored task graph (metadata columns only, no full JSON blob).
///
/// Returned by [`RawGraphStore::list_graphs`] for listing and filtering without
/// deserializing the full graph JSON.
#[derive(Debug, Clone)]
pub struct GraphSummary {
    /// String UUID of the graph.
    pub id: String,
    /// High-level user goal that was decomposed into this graph.
    pub goal: String,
    /// Lifecycle status string (e.g. `"completed"`, `"failed"`).
    pub status: String,
    /// ISO-8601 UTC creation timestamp.
    pub created_at: String,
    /// ISO-8601 UTC terminal timestamp, or `None` if the graph has not finished.
    pub finished_at: Option<String>,
}

/// Raw persistence interface for task graphs.
///
/// All graph data is stored as an opaque JSON blob. The orchestration layer in
/// `zeph-core` / `zeph-orchestration` is responsible for serializing and
/// deserializing `TaskGraph` values.
#[allow(async_fn_in_trait)]
pub trait RawGraphStore: Send + Sync {
    /// Persist a graph (upsert by `id`).
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] on database failure.
    async fn save_graph(
        &self,
        id: &str,
        goal: &str,
        status: &str,
        graph_json: &str,
        created_at: &str,
        finished_at: Option<&str>,
    ) -> Result<(), DbError>;

    /// Load a graph by its string UUID.
    ///
    /// Returns `None` if the graph does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] on database failure.
    async fn load_graph(&self, id: &str) -> Result<Option<String>, DbError>;

    /// List graphs ordered by `created_at` descending, limited to `limit` rows.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] on database failure.
    async fn list_graphs(&self, limit: u32) -> Result<Vec<GraphSummary>, DbError>;

    /// Delete a graph by its string UUID.
    ///
    /// Returns `true` if a row was deleted.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] on database failure.
    async fn delete_graph(&self, id: &str) -> Result<bool, DbError>;
}
