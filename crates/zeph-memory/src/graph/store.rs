// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::Stream;
use sqlx::SqlitePool;

use crate::error::MemoryError;
use crate::types::MessageId;

use super::types::{Community, Edge, Entity, EntityType};

pub struct GraphStore {
    pool: SqlitePool,
}

impl GraphStore {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── Entities ─────────────────────────────────────────────────────────────

    /// Insert or update an entity by `(name, entity_type)`. Updates `summary` and `last_seen_at`.
    ///
    /// Passing `summary = None` preserves the existing summary (via `COALESCE(excluded.summary, summary)`);
    /// it does not clear it. Pass `Some("")` to explicitly blank the summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn upsert_entity(
        &self,
        name: &str,
        entity_type: EntityType,
        summary: Option<&str>,
    ) -> Result<i64, MemoryError> {
        let type_str = entity_type.as_str();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_entities (name, entity_type, summary)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name, entity_type) DO UPDATE SET
               summary = COALESCE(excluded.summary, summary),
               last_seen_at = datetime('now')
             RETURNING id",
        )
        .bind(name)
        .bind(type_str)
        .bind(summary)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Find an entity by exact name and type.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity(
        &self,
        name: &str,
        entity_type: EntityType,
    ) -> Result<Option<Entity>, MemoryError> {
        let type_str = entity_type.as_str();
        let row: Option<EntityRow> = sqlx::query_as(
            "SELECT id, name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE name = ?1 AND entity_type = ?2",
        )
        .bind(name)
        .bind(type_str)
        .fetch_optional(&self.pool)
        .await?;
        row.map(entity_from_row).transpose()
    }

    /// Find entities whose name contains `query` (case-insensitive), up to `limit` results.
    ///
    /// Note: uses `LIKE '%query%'` with a leading wildcard, which bypasses the name B-tree index
    /// and performs a full table scan. Acceptable for Phase 1 (<10k entities); use FTS5 at scale.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entities_fuzzy(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryError> {
        // Escape LIKE metacharacters to prevent correctness bugs (e.g. "100%" matching "100 things").
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let limit = i64::try_from(limit)?;
        let rows: Vec<EntityRow> = sqlx::query_as(
            "SELECT id, name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE name LIKE ?1 ESCAPE '\\' COLLATE NOCASE
             ORDER BY last_seen_at DESC
             LIMIT ?2",
        )
        .bind(pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(entity_from_row)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Stream all entities from the database incrementally (true cursor, no full-table load).
    pub fn all_entities_stream(&self) -> impl Stream<Item = Result<Entity, MemoryError>> + '_ {
        use futures::StreamExt as _;
        sqlx::query_as::<_, EntityRow>(
            "SELECT id, name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities ORDER BY id ASC",
        )
        .fetch(&self.pool)
        .map(|r: Result<EntityRow, sqlx::Error>| {
            r.map_err(MemoryError::from).and_then(entity_from_row)
        })
    }

    /// Collect all entities into a Vec.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or `entity_type` parsing fails.
    pub async fn all_entities(&self) -> Result<Vec<Entity>, MemoryError> {
        use futures::TryStreamExt as _;
        self.all_entities_stream().try_collect().await
    }

    /// Count the total number of entities.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn entity_count(&self) -> Result<i64, MemoryError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM graph_entities")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    // ── Edges ─────────────────────────────────────────────────────────────────

    /// Insert a new edge between two entities.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn insert_edge(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<MessageId>,
    ) -> Result<i64, MemoryError> {
        let confidence = confidence.clamp(0.0, 1.0);
        let episode_raw: Option<i64> = episode_id.map(|m| m.0);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, episode_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
        )
        .bind(source_entity_id)
        .bind(target_entity_id)
        .bind(relation)
        .bind(fact)
        .bind(f64::from(confidence))
        .bind(episode_raw)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Mark an edge as invalid (set `valid_to` and `expired_at` to now).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn invalidate_edge(&self, edge_id: i64) -> Result<(), MemoryError> {
        sqlx::query(
            "UPDATE graph_edges SET valid_to = datetime('now'), expired_at = datetime('now')
             WHERE id = ?1",
        )
        .bind(edge_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get all active edges where entity is source or target.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_for_entity(&self, entity_id: i64) -> Result<Vec<Edge>, MemoryError> {
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id
             FROM graph_edges
             WHERE valid_to IS NULL
               AND (source_entity_id = ?1 OR target_entity_id = ?1)",
        )
        .bind(entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Get all active edges between two entities (both directions).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_between(
        &self,
        entity_a: i64,
        entity_b: i64,
    ) -> Result<Vec<Edge>, MemoryError> {
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id
             FROM graph_edges
             WHERE valid_to IS NULL
               AND ((source_entity_id = ?1 AND target_entity_id = ?2)
                 OR (source_entity_id = ?2 AND target_entity_id = ?1))",
        )
        .bind(entity_a)
        .bind(entity_b)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Get active edges from `source` to `target` in the exact direction (no reverse).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_exact(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
    ) -> Result<Vec<Edge>, MemoryError> {
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id
             FROM graph_edges
             WHERE valid_to IS NULL
               AND source_entity_id = ?1
               AND target_entity_id = ?2",
        )
        .bind(source_entity_id)
        .bind(target_entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Count active (non-invalidated) edges.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn active_edge_count(&self) -> Result<i64, MemoryError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM graph_edges WHERE valid_to IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    // ── Communities ───────────────────────────────────────────────────────────

    /// Insert or update a community by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or JSON serialization fails.
    pub async fn upsert_community(
        &self,
        name: &str,
        summary: &str,
        entity_ids: &[i64],
    ) -> Result<i64, MemoryError> {
        let entity_ids_json = serde_json::to_string(entity_ids)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_communities (name, summary, entity_ids)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET
               summary = excluded.summary,
               entity_ids = excluded.entity_ids,
               updated_at = datetime('now')
             RETURNING id",
        )
        .bind(name)
        .bind(summary)
        .bind(entity_ids_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Find the first community that contains the given `entity_id`.
    ///
    /// Uses `json_each()` to push the membership search into `SQLite`, avoiding a full
    /// table scan with per-row JSON parsing.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or JSON parsing fails.
    pub async fn community_for_entity(
        &self,
        entity_id: i64,
    ) -> Result<Option<Community>, MemoryError> {
        let row: Option<CommunityRow> = sqlx::query_as(
            "SELECT c.id, c.name, c.summary, c.entity_ids, c.created_at, c.updated_at
             FROM graph_communities c, json_each(c.entity_ids) j
             WHERE CAST(j.value AS INTEGER) = ?1
             LIMIT 1",
        )
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => {
                let entity_ids: Vec<i64> = serde_json::from_str(&row.entity_ids)?;
                Ok(Some(Community {
                    id: row.id,
                    name: row.name,
                    summary: row.summary,
                    entity_ids,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// Get all communities.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or JSON parsing fails.
    pub async fn all_communities(&self) -> Result<Vec<Community>, MemoryError> {
        let rows: Vec<CommunityRow> = sqlx::query_as(
            "SELECT id, name, summary, entity_ids, created_at, updated_at
             FROM graph_communities
             ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let entity_ids: Vec<i64> = serde_json::from_str(&row.entity_ids)?;
                Ok(Community {
                    id: row.id,
                    name: row.name,
                    summary: row.summary,
                    entity_ids,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    }

    /// Count the total number of communities.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn community_count(&self) -> Result<i64, MemoryError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM graph_communities")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    /// Get a metadata value by key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_metadata(&self, key: &str) -> Result<Option<String>, MemoryError> {
        let val: Option<String> =
            sqlx::query_scalar("SELECT value FROM graph_metadata WHERE key = ?1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(val)
    }

    /// Set a metadata value by key (upsert).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn set_metadata(&self, key: &str, value: &str) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO graph_metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── BFS Traversal ─────────────────────────────────────────────────────────

    /// Breadth-first traversal from `start_entity_id` up to `max_hops` hops.
    ///
    /// Returns all reachable entities and the active edges connecting them.
    /// Implements BFS iteratively in Rust to guarantee cycle safety regardless
    /// of `SQLite` CTE limitations.
    ///
    /// **`SQLite` bind parameter limit**: each BFS hop binds the frontier IDs three times in the
    /// neighbour query. At ~300+ frontier entities per hop, the IN clause may approach `SQLite`'s
    /// default `SQLITE_MAX_VARIABLE_NUMBER` limit of 999. Acceptable for Phase 1 (small graphs,
    /// `max_hops` typically 2–3). For large graphs, consider batching or a temp-table approach.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    pub async fn bfs(
        &self,
        start_entity_id: i64,
        max_hops: u32,
    ) -> Result<(Vec<Entity>, Vec<Edge>), MemoryError> {
        self.bfs_with_depth(start_entity_id, max_hops)
            .await
            .map(|(e, ed, _)| (e, ed))
    }

    /// BFS traversal returning entities, edges, and a depth map (`entity_id` → hop distance).
    ///
    /// The depth map records the minimum hop distance from `start_entity_id` to each visited
    /// entity. The start entity itself has depth 0.
    ///
    /// **`SQLite` bind parameter limit**: see [`bfs`] for notes on frontier size limits.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    pub async fn bfs_with_depth(
        &self,
        start_entity_id: i64,
        max_hops: u32,
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        use std::collections::HashMap;

        // SQLite binds frontier IDs 3× per hop; at >333 IDs the IN clause exceeds
        // SQLITE_MAX_VARIABLE_NUMBER (999). Cap to 300 to stay safely within the limit.
        const MAX_FRONTIER: usize = 300;

        let mut depth_map: HashMap<i64, u32> = HashMap::new();
        let mut frontier: Vec<i64> = vec![start_entity_id];
        depth_map.insert(start_entity_id, 0);

        for hop in 0..max_hops {
            if frontier.is_empty() {
                break;
            }
            frontier.truncate(MAX_FRONTIER);
            // IDs come from our own DB — no user input, no injection risk.
            let placeholders = frontier
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let neighbour_sql = format!(
                "SELECT DISTINCT CASE
                     WHEN source_entity_id IN ({placeholders}) THEN target_entity_id
                     ELSE source_entity_id
                 END as neighbour_id
                 FROM graph_edges
                 WHERE valid_to IS NULL
                   AND (source_entity_id IN ({placeholders}) OR target_entity_id IN ({placeholders}))"
            );
            let mut q = sqlx::query_scalar::<_, i64>(&neighbour_sql);
            for id in &frontier {
                q = q.bind(*id);
            }
            for id in &frontier {
                q = q.bind(*id);
            }
            for id in &frontier {
                q = q.bind(*id);
            }
            let neighbours: Vec<i64> = q.fetch_all(&self.pool).await?;

            let mut next_frontier: Vec<i64> = Vec::new();
            for nbr in neighbours {
                if let std::collections::hash_map::Entry::Vacant(e) = depth_map.entry(nbr) {
                    e.insert(hop + 1);
                    next_frontier.push(nbr);
                }
            }
            frontier = next_frontier;
        }

        let visited_ids: Vec<i64> = depth_map.keys().copied().collect();
        if visited_ids.is_empty() {
            return Ok((Vec::new(), Vec::new(), depth_map));
        }

        // Fetch edges between visited entities
        let placeholders = visited_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let edge_sql = format!(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id
             FROM graph_edges
             WHERE valid_to IS NULL
               AND source_entity_id IN ({placeholders})
               AND target_entity_id IN ({placeholders})"
        );
        let mut edge_query = sqlx::query_as::<_, EdgeRow>(&edge_sql);
        for id in &visited_ids {
            edge_query = edge_query.bind(*id);
        }
        for id in &visited_ids {
            edge_query = edge_query.bind(*id);
        }
        let edge_rows: Vec<EdgeRow> = edge_query.fetch_all(&self.pool).await?;

        let entity_sql = format!(
            "SELECT id, name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities WHERE id IN ({placeholders})"
        );
        let mut entity_query = sqlx::query_as::<_, EntityRow>(&entity_sql);
        for id in &visited_ids {
            entity_query = entity_query.bind(*id);
        }
        let entity_rows: Vec<EntityRow> = entity_query.fetch_all(&self.pool).await?;

        let entities: Vec<Entity> = entity_rows
            .into_iter()
            .map(entity_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let edges: Vec<Edge> = edge_rows.into_iter().map(edge_from_row).collect();

        Ok((entities, edges, depth_map))
    }
}

// ── Row types for sqlx::query_as ─────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct EntityRow {
    id: i64,
    name: String,
    entity_type: String,
    summary: Option<String>,
    first_seen_at: String,
    last_seen_at: String,
    qdrant_point_id: Option<String>,
}

fn entity_from_row(row: EntityRow) -> Result<Entity, MemoryError> {
    let entity_type = row
        .entity_type
        .parse::<EntityType>()
        .map_err(MemoryError::GraphStore)?;
    Ok(Entity {
        id: row.id,
        name: row.name,
        entity_type,
        summary: row.summary,
        first_seen_at: row.first_seen_at,
        last_seen_at: row.last_seen_at,
        qdrant_point_id: row.qdrant_point_id,
    })
}

#[derive(sqlx::FromRow)]
struct EdgeRow {
    id: i64,
    source_entity_id: i64,
    target_entity_id: i64,
    relation: String,
    fact: String,
    confidence: f64,
    valid_from: String,
    valid_to: Option<String>,
    created_at: String,
    expired_at: Option<String>,
    episode_id: Option<i64>,
    qdrant_point_id: Option<String>,
}

fn edge_from_row(row: EdgeRow) -> Edge {
    Edge {
        id: row.id,
        source_entity_id: row.source_entity_id,
        target_entity_id: row.target_entity_id,
        relation: row.relation,
        fact: row.fact,
        #[allow(clippy::cast_possible_truncation)]
        confidence: row.confidence as f32,
        valid_from: row.valid_from,
        valid_to: row.valid_to,
        created_at: row.created_at,
        expired_at: row.expired_at,
        episode_id: row.episode_id.map(MessageId),
        qdrant_point_id: row.qdrant_point_id,
    }
}

#[derive(sqlx::FromRow)]
struct CommunityRow {
    id: i64,
    name: String,
    summary: String,
    entity_ids: String,
    created_at: String,
    updated_at: String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;

    async fn setup() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    #[tokio::test]
    async fn upsert_entity_insert_new() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("Alice", EntityType::Person, Some("a person"))
            .await
            .unwrap();
        assert!(id > 0);
    }

    #[tokio::test]
    async fn upsert_entity_update_existing() {
        let gs = setup().await;
        let id1 = gs
            .upsert_entity("Alice", EntityType::Person, None)
            .await
            .unwrap();
        // Sleep 1ms to ensure datetime changes; SQLite datetime granularity is 1s,
        // so we verify idempotency instead of timestamp ordering.
        let id2 = gs
            .upsert_entity("Alice", EntityType::Person, Some("updated"))
            .await
            .unwrap();
        assert_eq!(id1, id2);
        let entity = gs
            .find_entity("Alice", EntityType::Person)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.summary.as_deref(), Some("updated"));
    }

    #[tokio::test]
    async fn find_entity_found() {
        let gs = setup().await;
        gs.upsert_entity("Bob", EntityType::Tool, Some("a tool"))
            .await
            .unwrap();
        let entity = gs
            .find_entity("Bob", EntityType::Tool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.name, "Bob");
        assert_eq!(entity.entity_type, EntityType::Tool);
    }

    #[tokio::test]
    async fn find_entity_not_found() {
        let gs = setup().await;
        let result = gs.find_entity("Nobody", EntityType::Person).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_entities_fuzzy_partial_match() {
        let gs = setup().await;
        gs.upsert_entity("GraphQL", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Graph", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Unrelated", EntityType::Concept, None)
            .await
            .unwrap();

        let results = gs.find_entities_fuzzy("graph", 10).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|e| e.name == "GraphQL"));
        assert!(results.iter().any(|e| e.name == "Graph"));
    }

    #[tokio::test]
    async fn entity_count_empty() {
        let gs = setup().await;
        assert_eq!(gs.entity_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn entity_count_non_empty() {
        let gs = setup().await;
        gs.upsert_entity("A", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("B", EntityType::Concept, None)
            .await
            .unwrap();
        assert_eq!(gs.entity_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn all_entities_and_stream() {
        let gs = setup().await;
        gs.upsert_entity("X", EntityType::Project, None)
            .await
            .unwrap();
        gs.upsert_entity("Y", EntityType::Language, None)
            .await
            .unwrap();

        let all = gs.all_entities().await.unwrap();
        assert_eq!(all.len(), 2);

        use futures::StreamExt as _;
        let streamed: Vec<Result<Entity, _>> = gs.all_entities_stream().collect().await;
        assert_eq!(streamed.len(), 2);
        assert!(streamed.iter().all(|r| r.is_ok()));
    }

    #[tokio::test]
    async fn insert_edge_without_episode() {
        let gs = setup().await;
        let src = gs
            .upsert_entity("Src", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("Tgt", EntityType::Concept, None)
            .await
            .unwrap();
        let eid = gs
            .insert_edge(src, tgt, "relates_to", "Src relates to Tgt", 0.9, None)
            .await
            .unwrap();
        assert!(eid > 0);
    }

    #[tokio::test]
    async fn insert_edge_with_episode() {
        let gs = setup().await;
        let src = gs
            .upsert_entity("Src2", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("Tgt2", EntityType::Concept, None)
            .await
            .unwrap();
        // Verifies that passing an episode_id does not cause a panic or unexpected error on the
        // insertion path itself. The episode_id references the messages table; whether the FK
        // constraint fires depends on the SQLite FK enforcement mode at runtime. Both success
        // (FK off) and FK-violation error are acceptable outcomes for this test — we only assert
        // that insert_edge does not panic or return an unexpected error type.
        let episode = MessageId(999);
        let result = gs
            .insert_edge(src, tgt, "uses", "Src2 uses Tgt2", 1.0, Some(episode))
            .await;
        match &result {
            Ok(eid) => assert!(*eid > 0, "inserted edge should have positive id"),
            Err(MemoryError::Sqlite(_)) => {} // FK constraint failed — acceptable
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn invalidate_edge_sets_timestamps() {
        let gs = setup().await;
        let src = gs
            .upsert_entity("E1", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("E2", EntityType::Concept, None)
            .await
            .unwrap();
        let eid = gs
            .insert_edge(src, tgt, "r", "fact", 1.0, None)
            .await
            .unwrap();
        gs.invalidate_edge(eid).await.unwrap();

        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT valid_to, expired_at FROM graph_edges WHERE id = ?1")
                .bind(eid)
                .fetch_one(&gs.pool)
                .await
                .unwrap();
        assert!(row.0.is_some(), "valid_to should be set");
        assert!(row.1.is_some(), "expired_at should be set");
    }

    #[tokio::test]
    async fn edges_for_entity_both_directions() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("A", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("B", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("C", EntityType::Concept, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(c, a, "r", "f2", 1.0, None).await.unwrap();

        let edges = gs.edges_for_entity(a).await.unwrap();
        assert_eq!(edges.len(), 2);
    }

    #[tokio::test]
    async fn edges_between_both_directions() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("PA", EntityType::Person, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("PB", EntityType::Person, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "knows", "PA knows PB", 1.0, None)
            .await
            .unwrap();

        let fwd = gs.edges_between(a, b).await.unwrap();
        assert_eq!(fwd.len(), 1);
        let rev = gs.edges_between(b, a).await.unwrap();
        assert_eq!(rev.len(), 1);
    }

    #[tokio::test]
    async fn active_edge_count_excludes_invalidated() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("N1", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("N2", EntityType::Concept, None)
            .await
            .unwrap();
        let e1 = gs.insert_edge(a, b, "r1", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(a, b, "r2", "f2", 1.0, None).await.unwrap();
        gs.invalidate_edge(e1).await.unwrap();

        assert_eq!(gs.active_edge_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn upsert_community_insert_and_update() {
        let gs = setup().await;
        let id1 = gs
            .upsert_community("clusterA", "summary A", &[1, 2, 3])
            .await
            .unwrap();
        assert!(id1 > 0);
        let id2 = gs
            .upsert_community("clusterA", "summary A updated", &[1, 2, 3, 4])
            .await
            .unwrap();
        assert_eq!(id1, id2);
        let communities = gs.all_communities().await.unwrap();
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0].summary, "summary A updated");
        assert_eq!(communities[0].entity_ids, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn community_for_entity_found() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("CA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("CB", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_community("cA", "summary", &[a, b]).await.unwrap();
        let result = gs.community_for_entity(a).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "cA");
    }

    #[tokio::test]
    async fn community_for_entity_not_found() {
        let gs = setup().await;
        let result = gs.community_for_entity(999).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn community_count() {
        let gs = setup().await;
        assert_eq!(gs.community_count().await.unwrap(), 0);
        gs.upsert_community("c1", "s1", &[]).await.unwrap();
        gs.upsert_community("c2", "s2", &[]).await.unwrap();
        assert_eq!(gs.community_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn metadata_get_set_round_trip() {
        let gs = setup().await;
        assert_eq!(gs.get_metadata("counter").await.unwrap(), None);
        gs.set_metadata("counter", "42").await.unwrap();
        assert_eq!(gs.get_metadata("counter").await.unwrap(), Some("42".into()));
        gs.set_metadata("counter", "43").await.unwrap();
        assert_eq!(gs.get_metadata("counter").await.unwrap(), Some("43".into()));
    }

    #[tokio::test]
    async fn bfs_max_hops_0_returns_only_start() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("BfsA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("BfsB", EntityType::Concept, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "r", "f", 1.0, None).await.unwrap();

        let (entities, edges) = gs.bfs(a, 0).await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].id, a);
        assert!(edges.is_empty());
    }

    #[tokio::test]
    async fn bfs_max_hops_2_chain() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("ChainA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("ChainB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("ChainC", EntityType::Concept, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();

        let (entities, edges) = gs.bfs(a, 2).await.unwrap();
        let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert!(ids.contains(&c));
        assert_eq!(edges.len(), 2);
    }

    #[tokio::test]
    async fn bfs_cycle_no_infinite_loop() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("CycA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("CycB", EntityType::Concept, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(b, a, "r", "f2", 1.0, None).await.unwrap();

        let (entities, _edges) = gs.bfs(a, 3).await.unwrap();
        let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
        // Should have exactly A and B, no infinite loop
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn test_invalidated_edges_excluded_from_bfs() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("InvA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("InvB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("InvC", EntityType::Concept, None)
            .await
            .unwrap();
        let ab = gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();
        // Invalidate A->B: BFS from A should not reach B or C.
        gs.invalidate_edge(ab).await.unwrap();

        let (entities, edges) = gs.bfs(a, 2).await.unwrap();
        let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![a], "only start entity should be reachable");
        assert!(edges.is_empty(), "no active edges should be returned");
    }

    #[tokio::test]
    async fn test_bfs_empty_graph() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("IsoA", EntityType::Concept, None)
            .await
            .unwrap();

        let (entities, edges) = gs.bfs(a, 2).await.unwrap();
        let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![a], "isolated node: only start entity returned");
        assert!(edges.is_empty(), "no edges for isolated node");
    }

    #[tokio::test]
    async fn test_bfs_diamond() {
        let gs = setup().await;
        let a = gs
            .upsert_entity("DiamA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("DiamB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("DiamC", EntityType::Concept, None)
            .await
            .unwrap();
        let d = gs
            .upsert_entity("DiamD", EntityType::Concept, None)
            .await
            .unwrap();
        gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
        gs.insert_edge(a, c, "r", "f2", 1.0, None).await.unwrap();
        gs.insert_edge(b, d, "r", "f3", 1.0, None).await.unwrap();
        gs.insert_edge(c, d, "r", "f4", 1.0, None).await.unwrap();

        let (entities, edges) = gs.bfs(a, 2).await.unwrap();
        let mut ids: Vec<_> = entities.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let mut expected = vec![a, b, c, d];
        expected.sort_unstable();
        assert_eq!(ids, expected, "all 4 nodes reachable, no duplicates");
        assert_eq!(edges.len(), 4, "all 4 edges returned");
    }

    #[tokio::test]
    async fn test_find_entities_fuzzy_no_results() {
        let gs = setup().await;
        gs.upsert_entity("Alpha", EntityType::Concept, None)
            .await
            .unwrap();
        let results = gs.find_entities_fuzzy("zzzznonexistent", 10).await.unwrap();
        assert!(
            results.is_empty(),
            "no entities should match an unknown term"
        );
    }
}
