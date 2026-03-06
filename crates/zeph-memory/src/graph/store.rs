// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::Stream;
use sqlx::SqlitePool;

use crate::error::MemoryError;
use crate::sqlite::messages::sanitize_fts5_query;
use crate::types::MessageId;

use super::types::{Community, Edge, Entity, EntityAlias, EntityType};

pub struct GraphStore {
    pool: SqlitePool,
}

impl GraphStore {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ── Entities ─────────────────────────────────────────────────────────────

    /// Insert or update an entity by `(canonical_name, entity_type)`.
    ///
    /// - `surface_name`: the original display form (e.g. `"Rust"`) — stored in the `name` column
    ///   so user-facing output preserves casing. Updated on every upsert to the latest seen form.
    /// - `canonical_name`: the stable normalized key (e.g. `"rust"`) — used for deduplication.
    /// - `summary`: pass `None` to preserve the existing summary; pass `Some("")` to blank it.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn upsert_entity(
        &self,
        surface_name: &str,
        canonical_name: &str,
        entity_type: EntityType,
        summary: Option<&str>,
    ) -> Result<i64, MemoryError> {
        let type_str = entity_type.as_str();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_entities (name, canonical_name, entity_type, summary)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(canonical_name, entity_type) DO UPDATE SET
               name = excluded.name,
               summary = COALESCE(excluded.summary, summary),
               last_seen_at = datetime('now')
             RETURNING id",
        )
        .bind(surface_name)
        .bind(canonical_name)
        .bind(type_str)
        .bind(summary)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Find an entity by exact canonical name and type.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity(
        &self,
        canonical_name: &str,
        entity_type: EntityType,
    ) -> Result<Option<Entity>, MemoryError> {
        let type_str = entity_type.as_str();
        let row: Option<EntityRow> = sqlx::query_as(
            "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE canonical_name = ?1 AND entity_type = ?2",
        )
        .bind(canonical_name)
        .bind(type_str)
        .fetch_optional(&self.pool)
        .await?;
        row.map(entity_from_row).transpose()
    }

    /// Find entities matching `query` in name, summary, or aliases, up to `limit` results, ranked by relevance.
    ///
    /// Uses FTS5 MATCH with prefix wildcards (`token*`) and bm25 ranking. Name matches are
    /// weighted 10x higher than summary matches. Also searches `graph_entity_aliases` for
    /// alias matches via a UNION query.
    ///
    /// # Behavioral note
    ///
    /// This replaces the previous `LIKE '%query%'` implementation. FTS5 prefix matching differs
    /// from substring matching: searching "SQL" will match "`SQLite`" (prefix) but NOT "`GraphQL`"
    /// (substring). Entity names are indexed as single tokens by the unicode61 tokenizer, so
    /// mid-word substrings are not matched. This is a known trade-off for index performance.
    ///
    /// Single-character queries (e.g., "a") are allowed and produce a broad prefix match ("a*").
    /// The `limit` parameter caps the result set. No minimum query length is enforced; if this
    /// causes noise in practice, add a minimum length guard at the call site.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entities_fuzzy(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryError> {
        // FTS5 boolean operator keywords (case-sensitive uppercase). Filtering these
        // prevents syntax errors when user input contains them as literal search terms
        // (e.g., "graph OR unrelated" must not produce "graph* OR* unrelated*").
        const FTS5_OPERATORS: &[&str] = &["AND", "OR", "NOT", "NEAR"];
        let query = &query[..query.floor_char_boundary(512)];
        // Sanitize input: split on non-alphanumeric characters, filter empty tokens,
        // append '*' to each token for FTS5 prefix matching ("graph" -> "graph*").
        let sanitized = sanitize_fts5_query(query);
        if sanitized.is_empty() {
            return Ok(vec![]);
        }
        let fts_query: String = sanitized
            .split_whitespace()
            .filter(|t| !FTS5_OPERATORS.contains(t))
            .map(|t| format!("{t}*"))
            .collect::<Vec<_>>()
            .join(" ");
        if fts_query.is_empty() {
            return Ok(vec![]);
        }

        let limit = i64::try_from(limit)?;
        // bm25(graph_entities_fts, 10.0, 1.0): name column weighted 10x over summary.
        // bm25() returns negative values; ORDER BY ASC puts best matches first.
        let rows: Vec<EntityRow> = sqlx::query_as(
            "SELECT DISTINCT e.id, e.name, e.canonical_name, e.entity_type, e.summary,
                    e.first_seen_at, e.last_seen_at, e.qdrant_point_id
             FROM graph_entities_fts fts
             JOIN graph_entities e ON e.id = fts.rowid
             WHERE graph_entities_fts MATCH ?1
             UNION
             SELECT e.id, e.name, e.canonical_name, e.entity_type, e.summary,
                    e.first_seen_at, e.last_seen_at, e.qdrant_point_id
             FROM graph_entity_aliases a
             JOIN graph_entities e ON e.id = a.entity_id
             WHERE a.alias_name LIKE ?2 ESCAPE '\\' COLLATE NOCASE
             LIMIT ?3",
        )
        .bind(&fts_query)
        .bind(format!(
            "%{}%",
            query
                .trim()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(entity_from_row)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Find an entity by its primary key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity_by_id(&self, id: i64) -> Result<Option<Entity>, MemoryError> {
        let row: Option<EntityRow> = sqlx::query_as(
            "SELECT id, name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(entity_from_row).transpose()
    }

    /// Stream all entities from the database incrementally (true cursor, no full-table load).
    pub fn all_entities_stream(&self) -> impl Stream<Item = Result<Entity, MemoryError>> + '_ {
        use futures::StreamExt as _;
        sqlx::query_as::<_, EntityRow>(
            "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities ORDER BY id ASC",
        )
        .fetch(&self.pool)
        .map(|r: Result<EntityRow, sqlx::Error>| {
            r.map_err(MemoryError::from).and_then(entity_from_row)
        })
    }

    // ── Alias methods ─────────────────────────────────────────────────────────

    /// Insert an alias for an entity (idempotent: duplicate alias is silently ignored via UNIQUE constraint).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn add_alias(&self, entity_id: i64, alias_name: &str) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT OR IGNORE INTO graph_entity_aliases (entity_id, alias_name) VALUES (?1, ?2)",
        )
        .bind(entity_id)
        .bind(alias_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find an entity by alias name and entity type (case-insensitive).
    ///
    /// Filters by `entity_type` to avoid cross-type alias collisions (S2 fix).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity_by_alias(
        &self,
        alias_name: &str,
        entity_type: EntityType,
    ) -> Result<Option<Entity>, MemoryError> {
        let type_str = entity_type.as_str();
        let row: Option<EntityRow> = sqlx::query_as(
            "SELECT e.id, e.name, e.canonical_name, e.entity_type, e.summary,
                    e.first_seen_at, e.last_seen_at, e.qdrant_point_id
             FROM graph_entity_aliases a
             JOIN graph_entities e ON e.id = a.entity_id
             WHERE a.alias_name = ?1 COLLATE NOCASE
               AND e.entity_type = ?2
             ORDER BY e.id ASC
             LIMIT 1",
        )
        .bind(alias_name)
        .bind(type_str)
        .fetch_optional(&self.pool)
        .await?;
        row.map(entity_from_row).transpose()
    }

    /// Get all aliases for an entity.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn aliases_for_entity(
        &self,
        entity_id: i64,
    ) -> Result<Vec<EntityAlias>, MemoryError> {
        let rows: Vec<AliasRow> = sqlx::query_as(
            "SELECT id, entity_id, alias_name, created_at
             FROM graph_entity_aliases
             WHERE entity_id = ?1
             ORDER BY id ASC",
        )
        .bind(entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(alias_from_row).collect())
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

        let mut visited_ids: Vec<i64> = depth_map.keys().copied().collect();
        if visited_ids.is_empty() {
            return Ok((Vec::new(), Vec::new(), depth_map));
        }
        // Edge query binds visited_ids twice — cap at 499 to stay under SQLite 999 limit.
        visited_ids.truncate(499);

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
            "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
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
    canonical_name: String,
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
        canonical_name: row.canonical_name,
        entity_type,
        summary: row.summary,
        first_seen_at: row.first_seen_at,
        last_seen_at: row.last_seen_at,
        qdrant_point_id: row.qdrant_point_id,
    })
}

#[derive(sqlx::FromRow)]
struct AliasRow {
    id: i64,
    entity_id: i64,
    alias_name: String,
    created_at: String,
}

fn alias_from_row(row: AliasRow) -> EntityAlias {
    EntityAlias {
        id: row.id,
        entity_id: row.entity_id,
        alias_name: row.alias_name,
        created_at: row.created_at,
    }
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
            .upsert_entity("Alice", "Alice", EntityType::Person, Some("a person"))
            .await
            .unwrap();
        assert!(id > 0);
    }

    #[tokio::test]
    async fn upsert_entity_update_existing() {
        let gs = setup().await;
        let id1 = gs
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();
        // Sleep 1ms to ensure datetime changes; SQLite datetime granularity is 1s,
        // so we verify idempotency instead of timestamp ordering.
        let id2 = gs
            .upsert_entity("Alice", "Alice", EntityType::Person, Some("updated"))
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
        gs.upsert_entity("Bob", "Bob", EntityType::Tool, Some("a tool"))
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
        gs.upsert_entity("GraphQL", "GraphQL", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
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
        gs.upsert_entity("A", "A", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("B", "B", EntityType::Concept, None)
            .await
            .unwrap();
        assert_eq!(gs.entity_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn all_entities_and_stream() {
        let gs = setup().await;
        gs.upsert_entity("X", "X", EntityType::Project, None)
            .await
            .unwrap();
        gs.upsert_entity("Y", "Y", EntityType::Language, None)
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
            .upsert_entity("Src", "Src", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("Tgt", "Tgt", EntityType::Concept, None)
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
            .upsert_entity("Src2", "Src2", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("Tgt2", "Tgt2", EntityType::Concept, None)
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
            .upsert_entity("E1", "E1", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("E2", "E2", EntityType::Concept, None)
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
            .upsert_entity("A", "A", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("B", "B", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("C", "C", EntityType::Concept, None)
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
            .upsert_entity("PA", "PA", EntityType::Person, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("PB", "PB", EntityType::Person, None)
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
            .upsert_entity("N1", "N1", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("N2", "N2", EntityType::Concept, None)
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
            .upsert_entity("CA", "CA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("CB", "CB", EntityType::Concept, None)
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
            .upsert_entity("BfsA", "BfsA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("BfsB", "BfsB", EntityType::Concept, None)
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
            .upsert_entity("ChainA", "ChainA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("ChainB", "ChainB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("ChainC", "ChainC", EntityType::Concept, None)
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
            .upsert_entity("CycA", "CycA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("CycB", "CycB", EntityType::Concept, None)
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
            .upsert_entity("InvA", "InvA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("InvB", "InvB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("InvC", "InvC", EntityType::Concept, None)
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
            .upsert_entity("IsoA", "IsoA", EntityType::Concept, None)
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
            .upsert_entity("DiamA", "DiamA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("DiamB", "DiamB", EntityType::Concept, None)
            .await
            .unwrap();
        let c = gs
            .upsert_entity("DiamC", "DiamC", EntityType::Concept, None)
            .await
            .unwrap();
        let d = gs
            .upsert_entity("DiamD", "DiamD", EntityType::Concept, None)
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
        gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
            .await
            .unwrap();
        let results = gs.find_entities_fuzzy("zzzznonexistent", 10).await.unwrap();
        assert!(
            results.is_empty(),
            "no entities should match an unknown term"
        );
    }

    // ── Canonicalization / alias tests ────────────────────────────────────────

    #[tokio::test]
    async fn upsert_entity_stores_canonical_name() {
        let gs = setup().await;
        gs.upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        let entity = gs
            .find_entity("rust", EntityType::Language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.canonical_name, "rust");
        assert_eq!(entity.name, "rust");
    }

    #[tokio::test]
    async fn add_alias_idempotent() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();
        // Second insert should succeed silently (INSERT OR IGNORE)
        gs.add_alias(id, "rust-lang").await.unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert_eq!(
            aliases
                .iter()
                .filter(|a| a.alias_name == "rust-lang")
                .count(),
            1
        );
    }

    // ── FTS5 fuzzy search tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn find_entities_fuzzy_matches_summary() {
        let gs = setup().await;
        gs.upsert_entity(
            "Rust",
            "Rust",
            EntityType::Language,
            Some("a systems programming language"),
        )
        .await
        .unwrap();
        gs.upsert_entity(
            "Go",
            "Go",
            EntityType::Language,
            Some("a compiled language by Google"),
        )
        .await
        .unwrap();
        // Search by summary word — should find "Rust" by "systems" in summary.
        let results = gs.find_entities_fuzzy("systems", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rust");
    }

    #[tokio::test]
    async fn find_entities_fuzzy_empty_query() {
        let gs = setup().await;
        gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
            .await
            .unwrap();
        // Empty query returns empty vec without hitting the database.
        let results = gs.find_entities_fuzzy("", 10).await.unwrap();
        assert!(results.is_empty(), "empty query should return no results");
        // Whitespace-only query also returns empty.
        let results = gs.find_entities_fuzzy("   ", 10).await.unwrap();
        assert!(
            results.is_empty(),
            "whitespace query should return no results"
        );
    }

    #[tokio::test]
    async fn find_entity_by_alias_case_insensitive() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust").await.unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();

        let found = gs
            .find_entity_by_alias("RUST-LANG", EntityType::Language)
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, id);
    }

    #[tokio::test]
    async fn find_entity_by_alias_returns_none_for_unknown() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust").await.unwrap();

        let found = gs
            .find_entity_by_alias("python", EntityType::Language)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn find_entity_by_alias_filters_by_entity_type() {
        // "python" alias for Language should NOT match when looking for Tool type
        let gs = setup().await;
        let lang_id = gs
            .upsert_entity("python", "python", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(lang_id, "python").await.unwrap();

        let found_tool = gs
            .find_entity_by_alias("python", EntityType::Tool)
            .await
            .unwrap();
        assert!(
            found_tool.is_none(),
            "cross-type alias collision must not occur"
        );

        let found_lang = gs
            .find_entity_by_alias("python", EntityType::Language)
            .await
            .unwrap();
        assert!(found_lang.is_some());
        assert_eq!(found_lang.unwrap().id, lang_id);
    }

    #[tokio::test]
    async fn aliases_for_entity_returns_all() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust").await.unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();
        gs.add_alias(id, "rustlang").await.unwrap();

        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert_eq!(aliases.len(), 3);
        let names: Vec<&str> = aliases.iter().map(|a| a.alias_name.as_str()).collect();
        assert!(names.contains(&"rust"));
        assert!(names.contains(&"rust-lang"));
        assert!(names.contains(&"rustlang"));
    }

    #[tokio::test]
    async fn find_entities_fuzzy_includes_aliases() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();
        gs.upsert_entity("python", "python", EntityType::Language, None)
            .await
            .unwrap();

        // "rust-lang" is an alias, not the entity name — fuzzy search should still find it
        let results = gs.find_entities_fuzzy("rust-lang", 10).await.unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().any(|e| e.id == id));
    }

    #[tokio::test]
    async fn orphan_alias_cleanup_on_entity_delete() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("rust", "rust", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id, "rust").await.unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();

        // Delete the entity directly (bypassing FK for test purposes)
        sqlx::query("DELETE FROM graph_entities WHERE id = ?1")
            .bind(id)
            .execute(&gs.pool)
            .await
            .unwrap();

        // ON DELETE CASCADE should have removed aliases
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(
            aliases.is_empty(),
            "aliases should cascade-delete with entity"
        );
    }

    /// Validates migration 024 backfill on a pre-canonicalization database state.
    ///
    /// Simulates a database at migration 021 state (no canonical_name, no aliases), inserts
    /// entities and edges, then applies the migration 024 SQL directly via a single acquired
    /// connection (required so that PRAGMA foreign_keys = OFF takes effect on the same
    /// connection that executes DROP TABLE). Verifies:
    /// - canonical_name is backfilled from name for all existing entities
    /// - initial aliases are seeded from entity names
    /// - graph_edges survive (FK cascade did not wipe them)
    #[tokio::test]
    async fn migration_024_backfill_preserves_entities_and_edges() {
        use sqlx::Acquire as _;
        use sqlx::ConnectOptions as _;
        use sqlx::sqlite::SqliteConnectOptions;

        // Open an in-memory SQLite database with FK enforcement enabled (matches production).
        // Pool size = 1 ensures all queries share the same underlying connection.
        let opts = SqliteConnectOptions::from_url(&"sqlite::memory:".parse().unwrap())
            .unwrap()
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();

        // Create pre-023 schema (migration 021 state): no canonical_name column.
        sqlx::query(
            "CREATE TABLE graph_entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                summary TEXT,
                first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                qdrant_point_id TEXT,
                UNIQUE(name, entity_type)
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE graph_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
                target_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
                relation TEXT NOT NULL,
                fact TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 1.0,
                valid_from TEXT NOT NULL DEFAULT (datetime('now')),
                valid_to TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                expired_at TEXT,
                episode_id INTEGER,
                qdrant_point_id TEXT
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create FTS5 table and triggers (migration 023 state).
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS graph_entities_fts USING fts5(
                name, summary, content='graph_entities', content_rowid='id',
                tokenize='unicode61 remove_diacritics 2'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
             BEGIN INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, '')); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
             BEGIN INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, '')); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
             BEGIN
                 INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
                 INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, ''));
             END",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert pre-existing entities and an edge.
        let alice_id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_entities (name, entity_type) VALUES ('Alice', 'person') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let rust_id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_entities (name, entity_type) VALUES ('Rust', 'language') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact)
             VALUES (?1, ?2, 'uses', 'Alice uses Rust')",
        )
        .bind(alice_id)
        .bind(rust_id)
        .execute(&pool)
        .await
        .unwrap();

        // Apply migration 024 on a single pinned connection so PRAGMA foreign_keys = OFF
        // takes effect on the same connection that executes DROP TABLE (required because
        // PRAGMA foreign_keys is per-connection, not per-transaction).
        let mut conn = pool.acquire().await.unwrap();
        let conn = conn.acquire().await.unwrap();

        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE graph_entities ADD COLUMN canonical_name TEXT")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("UPDATE graph_entities SET canonical_name = name WHERE canonical_name IS NULL")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE graph_entities_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                canonical_name TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                summary TEXT,
                first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                qdrant_point_id TEXT,
                UNIQUE(canonical_name, entity_type)
             )",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO graph_entities_new
                 (id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id)
             SELECT id, name, COALESCE(canonical_name, name), entity_type, summary,
                    first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query("DROP TABLE graph_entities")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE graph_entities_new RENAME TO graph_entities")
            .execute(&mut *conn)
            .await
            .unwrap();
        // Rebuild FTS5 triggers (dropped with the old table) and rebuild index.
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
             BEGIN INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, '')); END",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
             BEGIN INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, '')); END",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
             BEGIN
                 INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
                 INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, ''));
             END",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query("INSERT INTO graph_entities_fts(graph_entities_fts) VALUES('rebuild')")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE graph_entity_aliases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
                alias_name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(alias_name, entity_id)
             )",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT OR IGNORE INTO graph_entity_aliases (entity_id, alias_name)
             SELECT id, name FROM graph_entities",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await
            .unwrap();

        // Verify: canonical_name backfilled from name
        let alice_canon: String =
            sqlx::query_scalar("SELECT canonical_name FROM graph_entities WHERE id = ?1")
                .bind(alice_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            alice_canon, "Alice",
            "canonical_name should equal pre-migration name"
        );

        let rust_canon: String =
            sqlx::query_scalar("SELECT canonical_name FROM graph_entities WHERE id = ?1")
                .bind(rust_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            rust_canon, "Rust",
            "canonical_name should equal pre-migration name"
        );

        // Verify: aliases seeded
        let alice_aliases: Vec<String> =
            sqlx::query_scalar("SELECT alias_name FROM graph_entity_aliases WHERE entity_id = ?1")
                .bind(alice_id)
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert!(
            alice_aliases.contains(&"Alice".to_owned()),
            "initial alias should be seeded from entity name"
        );

        // Verify: graph_edges survived (FK cascade did not wipe them)
        let edge_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM graph_edges")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            edge_count, 1,
            "graph_edges must survive migration 024 table recreation"
        );
    }

    #[tokio::test]
    async fn find_entity_by_alias_same_alias_two_entities_deterministic() {
        // Two same-type entities share an alias — ORDER BY id ASC ensures first-registered wins.
        let gs = setup().await;
        let id1 = gs
            .upsert_entity("python-v2", "python-v2", EntityType::Language, None)
            .await
            .unwrap();
        let id2 = gs
            .upsert_entity("python-v3", "python-v3", EntityType::Language, None)
            .await
            .unwrap();
        gs.add_alias(id1, "python").await.unwrap();
        gs.add_alias(id2, "python").await.unwrap();

        // Both entities now have alias "python" — should return the first-registered (id1)
        let found = gs
            .find_entity_by_alias("python", EntityType::Language)
            .await
            .unwrap();
        assert!(found.is_some(), "should find an entity by shared alias");
        // ORDER BY e.id ASC guarantees deterministic result: first inserted wins
        assert_eq!(
            found.unwrap().id,
            id1,
            "first-registered entity should win on shared alias"
        );
    }

    // ── FTS5 search tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn find_entities_fuzzy_special_chars() {
        let gs = setup().await;
        gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
            .await
            .unwrap();
        // FTS5 special characters in query must not cause an error.
        let results = gs.find_entities_fuzzy("graph\"()*:^", 10).await.unwrap();
        // "graph" survives sanitization and matches.
        assert!(results.iter().any(|e| e.name == "Graph"));
    }

    #[tokio::test]
    async fn find_entities_fuzzy_prefix_match() {
        let gs = setup().await;
        gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("GraphQL", "GraphQL", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
            .await
            .unwrap();
        // "Gra" prefix should match both "Graph" and "GraphQL" via FTS5 "gra*".
        let results = gs.find_entities_fuzzy("Gra", 10).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|e| e.name == "Graph"));
        assert!(results.iter().any(|e| e.name == "GraphQL"));
    }

    #[tokio::test]
    async fn find_entities_fuzzy_fts5_operator_injection() {
        let gs = setup().await;
        gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
            .await
            .unwrap();
        gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
            .await
            .unwrap();
        // "graph OR unrelated" — sanitizer splits on non-alphanumeric chars,
        // yielding tokens ["graph", "OR", "unrelated"]. The FTS5_OPERATORS filter
        // removes "OR", producing "graph* unrelated*" (implicit AND).
        // No entity contains both token prefixes, so the result is empty.
        let results = gs
            .find_entities_fuzzy("graph OR unrelated", 10)
            .await
            .unwrap();
        assert!(
            results.is_empty(),
            "implicit AND of 'graph*' and 'unrelated*' should match no entity"
        );
    }

    #[tokio::test]
    async fn find_entities_fuzzy_after_entity_update() {
        let gs = setup().await;
        // Insert entity with initial summary.
        gs.upsert_entity(
            "Foo",
            "Foo",
            EntityType::Concept,
            Some("initial summary bar"),
        )
        .await
        .unwrap();
        // Update summary via upsert — triggers the FTS UPDATE trigger.
        gs.upsert_entity(
            "Foo",
            "Foo",
            EntityType::Concept,
            Some("updated summary baz"),
        )
        .await
        .unwrap();
        // Old summary term should not match.
        let old_results = gs.find_entities_fuzzy("bar", 10).await.unwrap();
        assert!(
            old_results.is_empty(),
            "old summary content should not match after update"
        );
        // New summary term should match.
        let new_results = gs.find_entities_fuzzy("baz", 10).await.unwrap();
        assert_eq!(new_results.len(), 1);
        assert_eq!(new_results[0].name, "Foo");
    }

    #[tokio::test]
    async fn find_entities_fuzzy_only_special_chars() {
        let gs = setup().await;
        gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
            .await
            .unwrap();
        // Queries consisting solely of FTS5 special characters produce no alphanumeric
        // tokens after sanitization, so the function returns early with an empty vec
        // rather than passing an empty or malformed MATCH expression to FTS5.
        let results = gs.find_entities_fuzzy("***", 10).await.unwrap();
        assert!(
            results.is_empty(),
            "only special chars should return no results"
        );
        let results = gs.find_entities_fuzzy("(((", 10).await.unwrap();
        assert!(results.is_empty(), "only parens should return no results");
        let results = gs.find_entities_fuzzy("\"\"\"", 10).await.unwrap();
        assert!(results.is_empty(), "only quotes should return no results");
    }
}
