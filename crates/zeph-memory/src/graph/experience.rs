// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experience memory store — records tool execution outcomes and evolution sweeps.
//!
//! [`ExperienceStore`] persists agent tool-call outcomes as `experience_nodes` and links
//! them into a temporal chain (`experience_edges`). The [`EvolutionSweepStats`] type
//! describes pruning results from [`ExperienceStore::evolution_sweep`].

use std::time::{SystemTime, UNIX_EPOCH};

use zeph_db::{DbPool, sql};

use crate::error::MemoryError;
use crate::graph::store::GraphStore;

/// Statistics from a single graph evolution sweep.
#[derive(Debug, Default)]
pub struct EvolutionSweepStats {
    /// Number of self-loop edges removed from the knowledge graph.
    pub pruned_self_loops: usize,
    /// Number of low-confidence zero-retrieval edges removed.
    pub pruned_low_confidence: usize,
}

/// Persistent store for experience memory nodes and edges.
///
/// Wraps the `experience_nodes`, `experience_edges`, and `experience_entity_links`
/// tables created by migration `076_experience_memory.sql`.
pub struct ExperienceStore {
    pool: DbPool,
}

impl ExperienceStore {
    /// Create a new experience store using the provided connection pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Record a tool execution outcome as an experience node.
    ///
    /// Returns the row ID of the newly inserted experience node.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the database insert fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_memory::graph::experience::ExperienceStore;
    /// # async fn demo(store: &ExperienceStore) -> Result<(), Box<dyn std::error::Error>> {
    /// let id = store
    ///     .record_tool_outcome("session-1", 3, "shell", "success", Some("exit 0"), None)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument(
        skip_all,
        name = "memory.experience.record",
        fields(tool_name, outcome)
    )]
    pub async fn record_tool_outcome(
        &self,
        session_id: &str,
        turn: i64,
        tool_name: &str,
        outcome: &str,
        detail: Option<&str>,
        error_ctx: Option<&str>,
    ) -> Result<i64, MemoryError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs().cast_signed());
        let id: i64 = zeph_db::query_scalar(sql!(
            "INSERT INTO experience_nodes
             (session_id, turn, tool_name, outcome, detail, error_ctx, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id"
        ))
        .bind(session_id)
        .bind(turn)
        .bind(tool_name)
        .bind(outcome)
        .bind(detail)
        .bind(error_ctx)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(MemoryError::from)?;
        Ok(id)
    }

    /// Link an experience node to one or more knowledge graph entities.
    ///
    /// Uses `INSERT OR IGNORE` to tolerate duplicate links gracefully.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if any database insert fails.
    pub async fn link_to_entities(
        &self,
        experience_id: i64,
        entity_ids: &[i64],
    ) -> Result<(), MemoryError> {
        let _span = tracing::info_span!("memory.experience.link_entities", experience_id).entered();
        for &entity_id in entity_ids {
            zeph_db::query(sql!(
                "INSERT OR IGNORE INTO experience_entity_links
                 (experience_id, entity_id) VALUES (?1, ?2)"
            ))
            .bind(experience_id)
            .bind(entity_id)
            .execute(&self.pool)
            .await
            .map_err(MemoryError::from)?;
        }
        Ok(())
    }

    /// Record a sequential `followed_by` link between two experience nodes.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the database insert fails.
    pub async fn link_sequential(&self, prev: i64, next: i64) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO experience_edges
             (source_exp_id, target_exp_id, relation)
             VALUES (?1, ?2, 'followed_by')"
        ))
        .bind(prev)
        .bind(next)
        .execute(&self.pool)
        .await
        .map_err(MemoryError::from)?;
        Ok(())
    }

    /// Run a graph evolution sweep on the knowledge graph.
    ///
    /// Performs two pruning passes on the `graph_edges` table via `graph_store`:
    /// 1. Removes self-loops (`source_entity_id = target_entity_id`).
    /// 2. Removes low-confidence edges with zero retrievals that have no expiry set.
    ///
    /// This is a maintenance operation; it never blocks the agent loop.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if any database operation fails.
    #[tracing::instrument(skip_all, name = "memory.experience.sweep")]
    pub async fn evolution_sweep(
        &self,
        graph_store: &GraphStore,
        confidence_threshold: f32,
    ) -> Result<EvolutionSweepStats, MemoryError> {
        let self_loops = zeph_db::query(sql!(
            "DELETE FROM graph_edges WHERE source_entity_id = target_entity_id"
        ))
        .execute(graph_store.pool())
        .await
        .map_err(MemoryError::from)?
        .rows_affected();

        let low_conf = zeph_db::query(sql!(
            "DELETE FROM graph_edges
             WHERE confidence < ?1 AND retrieval_count = 0 AND valid_to IS NULL"
        ))
        .bind(confidence_threshold)
        .execute(graph_store.pool())
        .await
        .map_err(MemoryError::from)?
        .rows_affected();

        Ok(EvolutionSweepStats {
            pruned_self_loops: usize::try_from(self_loops).unwrap_or(usize::MAX),
            pruned_low_confidence: usize::try_from(low_conf).unwrap_or(usize::MAX),
        })
    }
}
