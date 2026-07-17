// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::{ActiveDialect, query, query_as, query_scalar, sql};

use super::SqliteStore;
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

impl SqliteStore {
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
        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let raw = format!(
            "INSERT INTO memory_tree
                (level, parent_id, content, source_ids, token_count, consolidated_at)
             VALUES (?, ?, ?, ?, ?, {now})
             RETURNING id"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let (id,): (i64,) = query_as(sqlx::AssertSqlSafe(query_sql))
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
        // `consolidated_at`/`created_at` are `TIMESTAMPTZ` on Postgres (`TEXT` on SQLite);
        // project both through `Dialect::select_as_text`, aliased back to their original
        // names so `#[derive(sqlx::FromRow)]` still binds them into the `String`/`Option<String>`
        // fields below.
        let consolidated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("consolidated_at");
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        // Inner query SELECTs the batch by true LRU priority (least-recently-touched first,
        // `COALESCE(last_attempted_at, created_at)` treats a never-attempted leaf's own creation
        // as its initial "touch"); `consolidation_attempts`/`id` break exact-timestamp ties
        // deterministically. This is a genuine bounded-wait guarantee, not just a bias: a leaf
        // that keeps losing the race has its touch time frozen in the past while every
        // competing leaf's touch time keeps refreshing to "now" each time it's tried, so the
        // frozen leaf's relative priority strictly increases until it wins — even under a
        // sustained stream of brand-new arrivals (#6393 follow-up, critic S1/M1).
        //
        // The outer query re-sorts the selected batch by `created_at ASC` because
        // `cluster_by_similarity` requires that exact presentation order for deterministic
        // clustering (see the INVARIANT comment on `cluster_by_similarity` in
        // `semantic/tree_consolidation.rs`) — selection priority and presentation order are
        // deliberately decoupled.
        let raw = format!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    {consolidated_at_sel} AS consolidated_at, {created_at_sel} AS created_at
             FROM (
                 SELECT id, level, parent_id, content, source_ids, token_count,
                        consolidated_at, created_at
                 FROM memory_tree
                 WHERE level = 0 AND parent_id IS NULL
                 ORDER BY COALESCE(last_attempted_at, created_at) ASC,
                          consolidation_attempts ASC,
                          id ASC
                 LIMIT ?
             ) AS batch
             ORDER BY batch.created_at ASC, batch.id ASC"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let rows: Vec<MemoryTreeRow> = query_as(sqlx::AssertSqlSafe(query_sql))
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
        // `consolidated_at`/`created_at` are `TIMESTAMPTZ` on Postgres — see
        // `load_tree_leaves_unconsolidated`.
        let consolidated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("consolidated_at");
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        // Same true-LRU starvation guard as `load_tree_leaves_unconsolidated` (#6393), applied
        // to higher-level consolidation too since the same stuck-singleton pattern can occur
        // there.
        let raw = format!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    {consolidated_at_sel} AS consolidated_at, {created_at_sel} AS created_at
             FROM (
                 SELECT id, level, parent_id, content, source_ids, token_count,
                        consolidated_at, created_at
                 FROM memory_tree
                 WHERE level = ? AND parent_id IS NULL
                 ORDER BY COALESCE(last_attempted_at, created_at) ASC,
                          consolidation_attempts ASC,
                          id ASC
                 LIMIT ?
             ) AS batch
             ORDER BY batch.created_at ASC, batch.id ASC"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let rows: Vec<MemoryTreeRow> = query_as(sqlx::AssertSqlSafe(query_sql))
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

        // `consolidated_at`/`created_at` are `TIMESTAMPTZ` on Postgres — see
        // `load_tree_leaves_unconsolidated`.
        let consolidated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("consolidated_at");
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let raw = format!(
            "SELECT id, level, parent_id, content, source_ids, token_count,
                    {consolidated_at_sel} AS consolidated_at, {created_at_sel} AS created_at
             FROM memory_tree
             WHERE id = ?"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);

        for _ in 0..=max_level {
            let row: Option<MemoryTreeRow> = query_as(sqlx::AssertSqlSafe(query_sql.clone()))
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

        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let raw = format!(
            "UPDATE memory_tree
             SET parent_id = ?, consolidated_at = {now}
             WHERE id = ? AND parent_id IS NULL"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        for &child_id in child_ids {
            query(sqlx::AssertSqlSafe(query_sql.as_str()))
                .bind(parent_id)
                .bind(child_id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Record a failed consolidation attempt for a batch of nodes (#6393).
    ///
    /// Increments `consolidation_attempts` and refreshes `last_attempted_at` for every id in
    /// `node_ids`. Called after a sweep loads nodes but they do not end up consolidated (no
    /// cluster formed, or the cluster's merge/persist failed). `last_attempted_at` is read by
    /// `load_tree_leaves_unconsolidated`/`load_tree_level` as the primary LRU ordering key
    /// (`COALESCE(last_attempted_at, created_at) ASC`): resetting it to "now" pushes a
    /// just-failed node to the back of the priority queue, which is what guarantees every node
    /// is re-considered within a bounded number of sweeps regardless of how many new nodes keep
    /// arriving — a node that never wins the race has its touch time frozen further and further
    /// in the past relative to fresh arrivals, so its priority strictly increases until it wins.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn bump_consolidation_attempts(&self, node_ids: &[i64]) -> Result<(), MemoryError> {
        if node_ids.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool().begin().await?;

        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let raw = format!(
            "UPDATE memory_tree
             SET consolidation_attempts = consolidation_attempts + 1,
                 last_attempted_at = {now}
             WHERE id = ?"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        for &node_id in node_ids {
            query(sqlx::AssertSqlSafe(query_sql.as_str()))
                .bind(node_id)
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

        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let insert_raw = format!(
            "INSERT INTO memory_tree
                (level, content, source_ids, token_count, consolidated_at)
             VALUES (?, ?, ?, ?, {now})
             RETURNING id"
        );
        let insert_sql = zeph_db::rewrite_placeholders(&insert_raw);
        let (parent_id,): (i64,) = zeph_db::query_as(sqlx::AssertSqlSafe(insert_sql))
            .bind(level)
            .bind(summary)
            .bind(source_ids_json)
            .bind(token_count)
            .fetch_one(&mut *tx)
            .await?;

        let update_raw = format!(
            "UPDATE memory_tree
             SET parent_id = ?, consolidated_at = {now}
             WHERE id = ? AND parent_id IS NULL"
        );
        let update_sql = zeph_db::rewrite_placeholders(&update_raw);
        for &child_id in child_ids {
            zeph_db::query(sqlx::AssertSqlSafe(update_sql.as_str()))
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
        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let raw = format!(
            "UPDATE memory_tree_meta
             SET total_consolidations = total_consolidations + 1,
                 last_consolidation_at = {now},
                 updated_at = {now}
             WHERE id = 1"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        query(sqlx::AssertSqlSafe(query_sql))
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

    async fn make_store() -> SqliteStore {
        SqliteStore::with_pool_size(":memory:", 1)
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

    #[tokio::test]
    async fn load_tree_leaves_unconsolidated_empty_tree_is_empty() {
        let store = make_store().await;
        let leaves = store
            .load_tree_leaves_unconsolidated(10)
            .await
            .expect("load");
        assert!(leaves.is_empty());
    }

    #[tokio::test]
    async fn bump_consolidation_attempts_empty_slice_is_noop() {
        let store = make_store().await;
        store.bump_consolidation_attempts(&[]).await.expect("noop");
    }

    /// Backdates `last_attempted_at` for `ids` to an exact, controlled value — used to build
    /// deterministic multi-sweep timelines in tests without depending on real wall-clock gaps.
    async fn backdate_last_attempted(store: &SqliteStore, ids: &[i64], timestamp: &str) {
        for &id in ids {
            zeph_db::query(zeph_db::sql!(
                "UPDATE memory_tree SET last_attempted_at = ? WHERE id = ?"
            ))
            .bind(timestamp)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("backdate last_attempted_at");
        }
    }

    /// Backdates `created_at` for `ids` — see [`backdate_last_attempted`].
    async fn backdate_created_at(store: &SqliteStore, ids: &[i64], timestamp: &str) {
        for &id in ids {
            zeph_db::query(zeph_db::sql!(
                "UPDATE memory_tree SET created_at = ? WHERE id = ?"
            ))
            .bind(timestamp)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("backdate created_at");
        }
    }

    async fn load_ids(store: &SqliteStore, limit: usize) -> std::collections::HashSet<i64> {
        store
            .load_tree_leaves_unconsolidated(limit)
            .await
            .expect("load")
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    /// #6393 regression, hardened per code review (2026-07-17T18-50-48-review.md Critical #1):
    /// the original version of this test relied on a real bump landing in the same `SQLite`
    /// second as the following load/insert to pass — a same-second timestamp tie, not genuine
    /// LRU precedence, and empirically false once a real time gap exists (a just-bumped leaf's
    /// touch time is "now," which is *older* than anything created strictly afterward, so it
    /// correctly keeps winning until it is left behind by a *later* sweep touching something
    /// else). This version backdates every timestamp explicitly, so the assertions hold
    /// regardless of how fast the test executes, and it demonstrates the real, bounded (not
    /// immediate) starvation-prevention property across two sweep cycles: a bump does not lose
    /// to a leaf created around the same moment, but a leaf that predates every touch in the
    /// table eventually wins once the actively-cycling leaves are touched again without it.
    #[tokio::test]
    async fn bump_consolidation_attempts_prevents_permanent_starvation() {
        let store = make_store().await;

        let stuck_a = store.insert_tree_leaf("stuck a", 1).await.expect("stuck_a");
        let stuck_b = store.insert_tree_leaf("stuck b", 1).await.expect("stuck_b");

        // Sweep 1 touches (bumps) stuck_a/stuck_b — exercises the real method, then pins its
        // effect to a controlled, known value (T=1) so the rest of this test is deterministic.
        store
            .bump_consolidation_attempts(&[stuck_a, stuck_b])
            .await
            .expect("bump 1");
        backdate_last_attempted(&store, &[stuck_a, stuck_b], "2000-01-01 00:00:01").await;

        // `waiting` is created strictly *after* that touch (T=2). It does NOT win the very next
        // load — a just-touched leaf's timestamp is still older (smaller) than anything created
        // after it, so stuck_a/stuck_b correctly keep winning for now. This is the real,
        // "not immediate" property: a bump does not instantly lose to a leaf created around the
        // same moment.
        let waiting = store
            .insert_tree_leaf("waiting, created after sweep 1's touch", 1)
            .await
            .expect("waiting");
        backdate_created_at(&store, &[waiting], "2000-01-01 00:00:02").await;

        let batch1 = load_ids(&store, 1).await;
        assert!(
            batch1 == std::collections::HashSet::from([stuck_a])
                || batch1 == std::collections::HashSet::from([stuck_b]),
            "a just-touched stuck leaf must still outrank a leaf created immediately \
             afterward, got {batch1:?}"
        );

        // Sweep 2 touches stuck_a/stuck_b *again* (T=3), still without `waiting` ever being
        // touched. `waiting`'s frozen T=2 is now the oldest timestamp in the table and must
        // finally win: the bounded, real multi-sweep starvation guard, not a false single-bump
        // immediacy claim.
        store
            .bump_consolidation_attempts(&[stuck_a, stuck_b])
            .await
            .expect("bump 2");
        backdate_last_attempted(&store, &[stuck_a, stuck_b], "2000-01-01 00:00:03").await;

        let batch2 = load_ids(&store, 1).await;
        assert_eq!(
            batch2,
            std::collections::HashSet::from([waiting]),
            "a leaf that predates the actively-cycling leaves' most recent touch must win once \
             they are touched again without it ever being touched itself"
        );
    }

    /// #6393 follow-up (critic S1): the ordering must be a genuine bounded-wait guarantee, not
    /// just a bias that a sustained stream of brand-new leaves can defeat. A leaf whose last
    /// (failed) attempt was long ago must outrank leaves created moments ago, no matter how many
    /// of them arrive — because `COALESCE(last_attempted_at, created_at)` for the frozen leaf is
    /// far in the past, while every fresh arrival's virtual touch time is "now".
    #[tokio::test]
    async fn load_tree_leaves_unconsolidated_prioritizes_stale_touch_over_sustained_new_arrivals() {
        let store = make_store().await;

        let stuck = store
            .insert_tree_leaf("stuck singleton", 1)
            .await
            .expect("stuck");
        // Simulate a leaf that was tried once, long ago, and has been losing the race ever
        // since — its `last_attempted_at` never got refreshed because it never won a batch slot.
        zeph_db::query(zeph_db::sql!(
            "UPDATE memory_tree
             SET consolidation_attempts = 5, last_attempted_at = '2000-01-01 00:00:00'
             WHERE id = ?"
        ))
        .bind(stuck)
        .execute(store.pool())
        .await
        .expect("backdate stuck leaf");

        // A burst of brand-new leaves "arriving now" — the adversarial scenario where a naive
        // attempts-only ordering would let fresh arrivals monopolize every batch forever.
        for i in 0..5 {
            store
                .insert_tree_leaf(&format!("fresh leaf {i}"), 1)
                .await
                .expect("fresh");
        }

        let batch = store
            .load_tree_leaves_unconsolidated(1)
            .await
            .expect("load");
        assert_eq!(batch.len(), 1);
        assert_eq!(
            batch[0].id, stuck,
            "a leaf neglected since 2000 must outrank leaves created moments ago"
        );
    }

    /// #6393 follow-up: selection priority (LRU) and presentation order (`created_at ASC`) are
    /// deliberately decoupled — `cluster_by_similarity`'s greedy leader algorithm requires the
    /// returned `Vec` to stay in `created_at ASC` order for deterministic clustering, regardless
    /// of which rows the LRU ordering picked or in what priority.
    #[tokio::test]
    async fn load_tree_leaves_unconsolidated_returns_created_at_order_even_when_priority_differs() {
        let store = make_store().await;

        let older = store
            .insert_tree_leaf("older, but recently retried", 1)
            .await
            .expect("older");
        let newer = store
            .insert_tree_leaf("newer, never attempted", 1)
            .await
            .expect("newer");

        // Give `older` the lowest selection priority (touched "now") even though it has the
        // earlier `created_at` — both still fit in a batch_size=2 load.
        store
            .bump_consolidation_attempts(&[older])
            .await
            .expect("bump older");

        let batch = store
            .load_tree_leaves_unconsolidated(2)
            .await
            .expect("load");
        let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            [older, newer],
            "returned rows must stay in created_at ASC order regardless of selection priority"
        );
    }
}
