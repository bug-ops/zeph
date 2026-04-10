// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::{query, query_as, query_scalar, sql};

use super::DbStore;
use crate::error::MemoryError;

/// A single memory tree node row from the `memory_tree` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryTreeRow {
    pub id: i64,
    pub level: i64,
    pub parent_id: Option<i64>,
    pub content: String,
    pub source_ids: String,
    pub token_count: i64,
    pub consolidated_at: Option<String>,
    pub created_at: String,
}

impl DbStore {
    /// Insert a leaf node (level 0) into the memory tree.
    ///
    /// Returns the id of the new row.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn insert_tree_leaf(
        &self,
        content: &str,
        token_count: i64,
    ) -> Result<i64, MemoryError> {
        let (id,): (i64,) = query_as(sql!(
            "INSERT INTO memory_tree (level, content, token_count)
             VALUES (0, ?, ?)
             RETURNING id"
        ))
        .bind(content)
        .bind(token_count)
        .fetch_one(self.pool())
        .await?;

        Ok(id)
    }

    /// Insert a consolidated node at a given level.
    ///
    /// Returns the id of the new row.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn insert_tree_node(
        &self,
        level: i64,
        parent_id: Option<i64>,
        content: &str,
        source_ids: &str,
        token_count: i64,
    ) -> Result<i64, MemoryError> {
        let (id,): (i64,) = query_as(sql!(
            "INSERT INTO memory_tree
                (level, parent_id, content, source_ids, token_count, consolidated_at)
             VALUES (?, ?, ?, ?, ?, datetime('now'))
             RETURNING id"
        ))
        .bind(level)
        .bind(parent_id)
        .bind(content)
        .bind(source_ids)
        .bind(token_count)
        .fetch_one(self.pool())
        .await?;

        Ok(id)
    }

    /// Load unconsolidated leaf nodes (level 0 without a parent).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_tree_leaves_unconsolidated(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryTreeRow>, MemoryError> {
        let rows: Vec<MemoryTreeRow> = query_as(sql!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    consolidated_at, created_at
             FROM memory_tree
             WHERE level = 0 AND parent_id IS NULL
             ORDER BY created_at ASC
             LIMIT ?"
        ))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(self.pool())
        .await?;

        Ok(rows)
    }

    /// Load all nodes at a given level (for consolidation of higher levels).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_tree_level(
        &self,
        level: i64,
        limit: usize,
    ) -> Result<Vec<MemoryTreeRow>, MemoryError> {
        let rows: Vec<MemoryTreeRow> = query_as(sql!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    consolidated_at, created_at
             FROM memory_tree
             WHERE level = ? AND parent_id IS NULL
             ORDER BY created_at ASC
             LIMIT ?"
        ))
        .bind(level)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(self.pool())
        .await?;

        Ok(rows)
    }

    /// Traverse from a leaf up to `max_level`, returning all ancestor nodes.
    ///
    /// The result is ordered from leaf (level 0) to root (highest level).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn traverse_tree_up(
        &self,
        leaf_id: i64,
        max_level: i64,
    ) -> Result<Vec<MemoryTreeRow>, MemoryError> {
        // Walk up via parent_id chain, bounded by max_level.
        let mut result = Vec::new();
        let mut current_id = leaf_id;

        for _ in 0..=max_level {
            let row: Option<MemoryTreeRow> = query_as(sql!(
                "SELECT id, level, parent_id, content, source_ids, token_count,
                        consolidated_at, created_at
                 FROM memory_tree
                 WHERE id = ?"
            ))
            .bind(current_id)
            .fetch_optional(self.pool())
            .await?;

            match row {
                None => break,
                Some(r) => {
                    let next_id = r.parent_id;
                    result.push(r);
                    match next_id {
                        None => break,
                        Some(p) => current_id = p,
                    }
                }
            }
        }

        Ok(result)
    }

    /// Mark child nodes as consolidated by setting their `parent_id`.
    ///
    /// This runs inside a single transaction to prevent partial state.
    /// Per-cluster transactions (critic S2 fix): call this once per cluster,
    /// not once per full sweep.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn mark_nodes_consolidated(
        &self,
        child_ids: &[i64],
        parent_id: i64,
    ) -> Result<(), MemoryError> {
        if child_ids.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool().begin().await?;

        for &child_id in child_ids {
            query(sql!(
                "UPDATE memory_tree
                 SET parent_id = ?, consolidated_at = datetime('now')
                 WHERE id = ? AND parent_id IS NULL"
            ))
            .bind(parent_id)
            .bind(child_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Insert a parent node and mark its children as consolidated in one transaction.
    ///
    /// Both the `INSERT` of the parent and the `UPDATE` of all children happen inside a single
    /// `BEGIN … COMMIT`. A crash between the two operations therefore leaves no orphaned parent.
    ///
    /// # Errors
    ///
    /// Returns an error if any query inside the transaction fails (the transaction is rolled back).
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.consolidate", skip_all)
    )]
    pub async fn consolidate_cluster(
        &self,
        level: i64,
        summary: &str,
        source_ids_json: &str,
        token_count: i64,
        child_ids: &[i64],
    ) -> Result<i64, MemoryError> {
        if child_ids.is_empty() {
            return Err(MemoryError::InvalidInput(
                "child_ids must not be empty".into(),
            ));
        }

        let mut tx = self.pool().begin().await?;

        let (parent_id,): (i64,) = zeph_db::query_as(zeph_db::sql!(
            "INSERT INTO memory_tree
                (level, content, source_ids, token_count, consolidated_at)
             VALUES (?, ?, ?, ?, datetime('now'))
             RETURNING id"
        ))
        .bind(level)
        .bind(summary)
        .bind(source_ids_json)
        .bind(token_count)
        .fetch_one(&mut *tx)
        .await?;

        for &child_id in child_ids {
            zeph_db::query(zeph_db::sql!(
                "UPDATE memory_tree
                 SET parent_id = ?, consolidated_at = datetime('now')
                 WHERE id = ? AND parent_id IS NULL"
            ))
            .bind(parent_id)
            .bind(child_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(parent_id)
    }

    /// Increment the total consolidation counter in `memory_tree_meta`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn increment_tree_consolidation_count(&self) -> Result<(), MemoryError> {
        query(sql!(
            "UPDATE memory_tree_meta
             SET total_consolidations = total_consolidations + 1,
                 last_consolidation_at = datetime('now'),
                 updated_at = datetime('now')
             WHERE id = 1"
        ))
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Count total nodes in the memory tree.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_tree_nodes(&self) -> Result<i64, MemoryError> {
        let count: i64 = query_scalar(sql!("SELECT COUNT(*) FROM memory_tree"))
            .fetch_one(self.pool())
            .await?;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> DbStore {
        DbStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store")
    }

    #[tokio::test]
    async fn insert_leaf_and_count() {
        let store = make_store().await;
        let id = store
            .insert_tree_leaf("remember this fact", 10)
            .await
            .expect("insert leaf");
        assert!(id > 0);
        assert_eq!(store.count_tree_nodes().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn load_unconsolidated_leaves_excludes_parented() {
        let store = make_store().await;
        let leaf1 = store.insert_tree_leaf("leaf one", 5).await.expect("leaf1");
        let leaf2 = store.insert_tree_leaf("leaf two", 5).await.expect("leaf2");

        // Consolidate into a parent node.
        let parent_id = store
            .insert_tree_node(1, None, "summary of leaf1 and leaf2", "[]", 10)
            .await
            .expect("parent");
        store
            .mark_nodes_consolidated(&[leaf1, leaf2], parent_id)
            .await
            .expect("mark consolidated");

        // No unconsolidated leaves should remain.
        let leaves = store
            .load_tree_leaves_unconsolidated(10)
            .await
            .expect("load");
        assert!(
            leaves.is_empty(),
            "consolidated leaves must not appear in unconsolidated query"
        );
    }

    #[tokio::test]
    async fn mark_nodes_consolidated_is_per_cluster_transaction() {
        let store = make_store().await;
        let leaf1 = store.insert_tree_leaf("a", 1).await.expect("l1");
        let leaf2 = store.insert_tree_leaf("b", 1).await.expect("l2");
        let parent = store
            .insert_tree_node(1, None, "ab summary", "[]", 2)
            .await
            .expect("parent");

        store
            .mark_nodes_consolidated(&[leaf1, leaf2], parent)
            .await
            .expect("mark");

        // Verify both are now parented.
        let rows: Vec<MemoryTreeRow> = zeph_db::query_as(zeph_db::sql!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    consolidated_at, created_at
             FROM memory_tree WHERE level = 0"
        ))
        .fetch_all(store.pool())
        .await
        .expect("fetch");

        assert!(rows.iter().all(|r| r.parent_id == Some(parent)));
    }

    #[tokio::test]
    async fn traverse_tree_up_returns_path() {
        let store = make_store().await;
        let leaf = store.insert_tree_leaf("leaf", 1).await.expect("leaf");
        let mid = store
            .insert_tree_node(1, None, "mid", "[]", 2)
            .await
            .expect("mid");
        store
            .mark_nodes_consolidated(&[leaf], mid)
            .await
            .expect("mark l→m");

        let path = store.traverse_tree_up(leaf, 3).await.expect("traverse");
        assert_eq!(path.len(), 2, "leaf + mid parent");
        assert_eq!(path[0].id, leaf);
        assert_eq!(path[1].id, mid);
    }

    #[tokio::test]
    async fn mark_nodes_consolidated_empty_slice_is_noop() {
        let store = make_store().await;
        // Should not fail on empty slice.
        store.mark_nodes_consolidated(&[], 999).await.expect("noop");
    }
}
