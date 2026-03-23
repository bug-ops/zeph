// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use futures::Stream;
use sqlx::SqlitePool;

use crate::error::MemoryError;
use crate::sqlite::messages::sanitize_fts5_query;
use crate::types::MessageId;

use super::types::{Community, Edge, EdgeType, Entity, EntityAlias, EntityType};

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

    /// Find an entity by its numeric ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity_by_id(&self, entity_id: i64) -> Result<Option<Entity>, MemoryError> {
        let row: Option<EntityRow> = sqlx::query_as(
            "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE id = ?1",
        )
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(entity_from_row).transpose()
    }

    /// Update the `qdrant_point_id` for an entity.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn set_entity_qdrant_point_id(
        &self,
        entity_id: i64,
        point_id: &str,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE graph_entities SET qdrant_point_id = ?1 WHERE id = ?2")
            .bind(point_id)
            .bind(entity_id)
            .execute(&self.pool)
            .await?;
        Ok(())
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

    /// Flush the `SQLite` WAL to the main database file.
    ///
    /// Runs `PRAGMA wal_checkpoint(PASSIVE)`. Safe to call at any time; does not block active
    /// readers or writers. Call after bulk entity inserts to ensure FTS5 shadow table writes are
    /// visible to connections opened in future sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the PRAGMA execution fails.
    pub async fn checkpoint_wal(&self) -> Result<(), MemoryError> {
        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&self.pool)
            .await?;
        Ok(())
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

    /// Insert a new edge between two entities, or update the existing active edge.
    ///
    /// An active edge is identified by `(source_entity_id, target_entity_id, relation, edge_type)`
    /// with `valid_to IS NULL`. If such an edge already exists, its `confidence` is updated to the
    /// maximum of the stored and incoming values, and the existing id is returned. This prevents
    /// duplicate edges from repeated extraction of the same context messages.
    ///
    /// The dedup key includes `edge_type` (critic mitigation): the same `(source, target, relation)`
    /// triple can legitimately exist with different edge types (e.g., `depends_on` can be both
    /// Semantic and Causal). Without `edge_type` in the key, the second insertion would silently
    /// update the first and lose the type classification.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn insert_edge(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<MessageId>,
    ) -> Result<i64, MemoryError> {
        self.insert_edge_typed(
            source_entity_id,
            target_entity_id,
            relation,
            fact,
            confidence,
            episode_id,
            EdgeType::Semantic,
        )
        .await
    }

    /// Insert a typed edge between two entities, or update the existing active edge of the same type.
    ///
    /// Identical semantics to [`insert_edge`] but with an explicit `edge_type` parameter.
    /// The dedup key is `(source_entity_id, target_entity_id, relation, edge_type, valid_to IS NULL)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_edge_typed(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<MessageId>,
        edge_type: EdgeType,
    ) -> Result<i64, MemoryError> {
        let confidence = confidence.clamp(0.0, 1.0);
        let edge_type_str = edge_type.as_str();

        let existing: Option<(i64, f64)> = sqlx::query_as(
            "SELECT id, confidence FROM graph_edges
             WHERE source_entity_id = ?1
               AND target_entity_id = ?2
               AND relation = ?3
               AND edge_type = ?4
               AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(source_entity_id)
        .bind(target_entity_id)
        .bind(relation)
        .bind(edge_type_str)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((existing_id, stored_conf)) = existing {
            let updated_conf = f64::from(confidence).max(stored_conf);
            sqlx::query("UPDATE graph_edges SET confidence = ?1 WHERE id = ?2")
                .bind(updated_conf)
                .bind(existing_id)
                .execute(&self.pool)
                .await?;
            return Ok(existing_id);
        }

        let episode_raw: Option<i64> = episode_id.map(|m| m.0);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_edges
             (source_entity_id, target_entity_id, relation, fact, confidence, episode_id, edge_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
        )
        .bind(source_entity_id)
        .bind(target_entity_id)
        .bind(relation)
        .bind(fact)
        .bind(f64::from(confidence))
        .bind(episode_raw)
        .bind(edge_type_str)
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

    /// Get all active edges for a batch of entity IDs, with optional MAGMA edge type filtering.
    ///
    /// Fetches all currently-active edges (`valid_to IS NULL`) where either endpoint
    /// is in `entity_ids`. Traversal is always current-time only (no `at_timestamp` support
    /// in v1 — see `bfs_at_timestamp` for historical traversal).
    ///
    /// # `SQLite` bind limit safety
    ///
    /// `SQLite` limits the number of bind parameters to `SQLITE_MAX_VARIABLE_NUMBER` (999 by
    /// default). Each entity ID requires two bind slots (source OR target), so batches are
    /// chunked at `MAX_BATCH_ENTITIES = 490` to stay safely under the limit regardless of
    /// compile-time `SQLite` configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_for_entities(
        &self,
        entity_ids: &[i64],
        edge_types: &[super::types::EdgeType],
    ) -> Result<Vec<Edge>, MemoryError> {
        // Safe margin under SQLite SQLITE_MAX_VARIABLE_NUMBER (999):
        // each entity ID uses 2 bind slots (source_entity_id OR target_entity_id).
        // 490 * 2 = 980, leaving headroom for future query additions.
        const MAX_BATCH_ENTITIES: usize = 490;

        let mut all_edges: Vec<Edge> = Vec::new();

        for chunk in entity_ids.chunks(MAX_BATCH_ENTITIES) {
            let edges = self.query_batch_edges(chunk, edge_types).await?;
            all_edges.extend(edges);
        }

        Ok(all_edges)
    }

    /// Query active edges for a single chunk of entity IDs (internal helper).
    ///
    /// Caller is responsible for ensuring `entity_ids.len() <= MAX_BATCH_ENTITIES`.
    async fn query_batch_edges(
        &self,
        entity_ids: &[i64],
        edge_types: &[super::types::EdgeType],
    ) -> Result<Vec<Edge>, MemoryError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Build a parameterized IN clause: (?1, ?2, ..., ?N).
        // We cannot use sqlx's query_as! macro here because the placeholder count is dynamic.
        let placeholders: String = (1..=entity_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = if edge_types.is_empty() {
            format!(
                "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                        valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                        edge_type
                 FROM graph_edges
                 WHERE valid_to IS NULL
                   AND (source_entity_id IN ({placeholders}) OR target_entity_id IN ({placeholders}))"
            )
        } else {
            let type_placeholders: String = (entity_ids.len() + 1
                ..=entity_ids.len() + edge_types.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                        valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                        edge_type
                 FROM graph_edges
                 WHERE valid_to IS NULL
                   AND (source_entity_id IN ({placeholders}) OR target_entity_id IN ({placeholders}))
                   AND edge_type IN ({type_placeholders})"
            )
        };

        // Bind entity IDs once — ?1..?N are reused for both IN clauses via ?NNN syntax.
        let mut query = sqlx::query_as::<_, EdgeRow>(&sql);
        for id in entity_ids {
            query = query.bind(*id);
        }
        for et in edge_types {
            query = query.bind(et.as_str());
        }

        let rows: Vec<EdgeRow> = query.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Get all active edges where entity is source or target.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_for_entity(&self, entity_id: i64) -> Result<Vec<Edge>, MemoryError> {
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE valid_to IS NULL
               AND (source_entity_id = ?1 OR target_entity_id = ?1)",
        )
        .bind(entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Get all edges (active and expired) where entity is source or target, ordered by
    /// `valid_from DESC`. Used by the `/graph history <name>` slash command.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or if `limit` overflows `i64`.
    pub async fn edge_history_for_entity(
        &self,
        entity_id: i64,
        limit: usize,
    ) -> Result<Vec<Edge>, MemoryError> {
        let limit = i64::try_from(limit)?;
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE source_entity_id = ?1 OR target_entity_id = ?1
             ORDER BY valid_from DESC
             LIMIT ?2",
        )
        .bind(entity_id)
        .bind(limit)
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
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
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
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
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

    /// Return per-type active edge counts as `(edge_type, count)` pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edge_type_distribution(&self) -> Result<Vec<(String, i64)>, MemoryError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT edge_type, COUNT(*) FROM graph_edges WHERE valid_to IS NULL GROUP BY edge_type ORDER BY edge_type",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // ── Communities ───────────────────────────────────────────────────────────

    /// Insert or update a community by name.
    ///
    /// `fingerprint` is a BLAKE3 hex string computed from sorted entity IDs and
    /// intra-community edge IDs. Pass `None` to leave the fingerprint unchanged (e.g. when
    /// `assign_to_community` adds an entity without a full re-detection pass).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or JSON serialization fails.
    pub async fn upsert_community(
        &self,
        name: &str,
        summary: &str,
        entity_ids: &[i64],
        fingerprint: Option<&str>,
    ) -> Result<i64, MemoryError> {
        let entity_ids_json = serde_json::to_string(entity_ids)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO graph_communities (name, summary, entity_ids, fingerprint)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               summary = excluded.summary,
               entity_ids = excluded.entity_ids,
               fingerprint = COALESCE(excluded.fingerprint, fingerprint),
               updated_at = datetime('now')
             RETURNING id",
        )
        .bind(name)
        .bind(summary)
        .bind(entity_ids_json)
        .bind(fingerprint)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Return a map of `fingerprint -> community_id` for all communities with a non-NULL
    /// fingerprint. Used by `detect_communities` to skip unchanged partitions.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn community_fingerprints(&self) -> Result<HashMap<String, i64>, MemoryError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT fingerprint, id FROM graph_communities WHERE fingerprint IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Delete a single community by its primary key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn delete_community_by_id(&self, id: i64) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM graph_communities WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set the fingerprint of a community to `NULL`, invalidating the incremental cache.
    ///
    /// Used by `assign_to_community` when an entity is added without a full re-detection pass,
    /// ensuring the next `detect_communities` run re-summarizes the affected community.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn clear_community_fingerprint(&self, id: i64) -> Result<(), MemoryError> {
        sqlx::query("UPDATE graph_communities SET fingerprint = NULL WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
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
            "SELECT c.id, c.name, c.summary, c.entity_ids, c.fingerprint, c.created_at, c.updated_at
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
                    fingerprint: row.fingerprint,
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
            "SELECT id, name, summary, entity_ids, fingerprint, created_at, updated_at
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
                    fingerprint: row.fingerprint,
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

    /// Get the current extraction count from metadata.
    ///
    /// Returns 0 if the counter has not been initialized.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn extraction_count(&self) -> Result<i64, MemoryError> {
        let val = self.get_metadata("extraction_count").await?;
        Ok(val.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0))
    }

    /// Stream all active (non-invalidated) edges.
    pub fn all_active_edges_stream(&self) -> impl Stream<Item = Result<Edge, MemoryError>> + '_ {
        use futures::StreamExt as _;
        sqlx::query_as::<_, EdgeRow>(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE valid_to IS NULL
             ORDER BY id ASC",
        )
        .fetch(&self.pool)
        .map(|r| r.map_err(MemoryError::from).map(edge_from_row))
    }

    /// Fetch a chunk of active edges using keyset pagination.
    ///
    /// Returns edges with `id > after_id` in ascending order, up to `limit` rows.
    /// Starting with `after_id = 0` returns the first chunk. Pass the last `id` from
    /// the returned chunk as `after_id` for the next page. An empty result means all
    /// edges have been consumed.
    ///
    /// Keyset pagination is O(1) per page (index seek on `id`) vs OFFSET which is O(N).
    /// It is also stable under concurrent inserts: new edges get monotonically higher IDs
    /// and will appear in subsequent chunks or after the last chunk, never causing
    /// duplicates. Concurrent invalidations (setting `valid_to`) may cause a single edge
    /// to be skipped, which is acceptable — LPA operates on an eventual-consistency snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_after_id(
        &self,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<Edge>, MemoryError> {
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE valid_to IS NULL AND id > ?1
             ORDER BY id ASC
             LIMIT ?2",
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Find a community by its primary key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or JSON parsing fails.
    pub async fn find_community_by_id(&self, id: i64) -> Result<Option<Community>, MemoryError> {
        let row: Option<CommunityRow> = sqlx::query_as(
            "SELECT id, name, summary, entity_ids, fingerprint, created_at, updated_at
             FROM graph_communities
             WHERE id = ?1",
        )
        .bind(id)
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
                    fingerprint: row.fingerprint,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// Delete all communities (full rebuild before upsert).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn delete_all_communities(&self) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM graph_communities")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete expired edges older than `retention_days` and return count deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn delete_expired_edges(&self, retention_days: u32) -> Result<usize, MemoryError> {
        let days = i64::from(retention_days);
        let result = sqlx::query(
            "DELETE FROM graph_edges
             WHERE expired_at IS NOT NULL
               AND expired_at < datetime('now', '-' || ?1 || ' days')",
        )
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(usize::try_from(result.rows_affected())?)
    }

    /// Delete orphan entities (no active edges, last seen more than `retention_days` ago).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn delete_orphan_entities(&self, retention_days: u32) -> Result<usize, MemoryError> {
        let days = i64::from(retention_days);
        let result = sqlx::query(
            "DELETE FROM graph_entities
             WHERE id NOT IN (
                 SELECT DISTINCT source_entity_id FROM graph_edges WHERE valid_to IS NULL
                 UNION
                 SELECT DISTINCT target_entity_id FROM graph_edges WHERE valid_to IS NULL
             )
             AND last_seen_at < datetime('now', '-' || ?1 || ' days')",
        )
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(usize::try_from(result.rows_affected())?)
    }

    /// Delete the oldest excess entities when count exceeds `max_entities`.
    ///
    /// Entities are ranked by ascending edge count, then ascending `last_seen_at` (LRU).
    /// Only deletes when `entity_count() > max_entities`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn cap_entities(&self, max_entities: usize) -> Result<usize, MemoryError> {
        let current = self.entity_count().await?;
        let max = i64::try_from(max_entities)?;
        if current <= max {
            return Ok(0);
        }
        let excess = current - max;
        let result = sqlx::query(
            "DELETE FROM graph_entities
             WHERE id IN (
                 SELECT e.id
                 FROM graph_entities e
                 LEFT JOIN (
                     SELECT source_entity_id AS eid, COUNT(*) AS cnt
                     FROM graph_edges WHERE valid_to IS NULL GROUP BY source_entity_id
                     UNION ALL
                     SELECT target_entity_id AS eid, COUNT(*) AS cnt
                     FROM graph_edges WHERE valid_to IS NULL GROUP BY target_entity_id
                 ) edge_counts ON e.id = edge_counts.eid
                 ORDER BY COALESCE(edge_counts.cnt, 0) ASC, e.last_seen_at ASC
                 LIMIT ?1
             )",
        )
        .bind(excess)
        .execute(&self.pool)
        .await?;
        Ok(usize::try_from(result.rows_affected())?)
    }

    // ── Temporal Edge Queries ─────────────────────────────────────────────────

    /// Return all edges for `entity_id` (as source or target) that were valid at `timestamp`.
    ///
    /// An edge is valid at `timestamp` when:
    /// - `valid_from <= timestamp`, AND
    /// - `valid_to IS NULL` (open-ended) OR `valid_to > timestamp`.
    ///
    /// `timestamp` must be a `SQLite` datetime string: `"YYYY-MM-DD HH:MM:SS"`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edges_at_timestamp(
        &self,
        entity_id: i64,
        timestamp: &str,
    ) -> Result<Vec<Edge>, MemoryError> {
        // Split into two UNIONed branches to leverage the partial indexes from migration 030:
        //   Branch 1 (active edges):     idx_graph_edges_valid + idx_graph_edges_source/target
        //   Branch 2 (historical edges): idx_graph_edges_src_temporal / idx_graph_edges_tgt_temporal
        let rows: Vec<EdgeRow> = sqlx::query_as(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE valid_to IS NULL
               AND valid_from <= ?2
               AND (source_entity_id = ?1 OR target_entity_id = ?1)
             UNION ALL
             SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE valid_to IS NOT NULL
               AND valid_from <= ?2
               AND valid_to > ?2
               AND (source_entity_id = ?1 OR target_entity_id = ?1)",
        )
        .bind(entity_id)
        .bind(timestamp)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(edge_from_row).collect())
    }

    /// Return all edge versions (active and expired) for the given `(source, predicate)` pair.
    ///
    /// The optional `relation` filter restricts results to a specific relation label.
    /// Results are ordered by `valid_from DESC` (most recent first).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn edge_history(
        &self,
        source_entity_id: i64,
        predicate: &str,
        relation: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Edge>, MemoryError> {
        // Escape LIKE wildcards so `%` and `_` in the predicate are treated as literals.
        let escaped = predicate
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let like_pattern = format!("%{escaped}%");
        let limit = i64::try_from(limit)?;
        let rows: Vec<EdgeRow> = if let Some(rel) = relation {
            sqlx::query_as(
                "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                        valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                        edge_type
                 FROM graph_edges
                 WHERE source_entity_id = ?1
                   AND fact LIKE ?2 ESCAPE '\\'
                   AND relation = ?3
                 ORDER BY valid_from DESC
                 LIMIT ?4",
            )
            .bind(source_entity_id)
            .bind(&like_pattern)
            .bind(rel)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                        valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                        edge_type
                 FROM graph_edges
                 WHERE source_entity_id = ?1
                   AND fact LIKE ?2 ESCAPE '\\'
                 ORDER BY valid_from DESC
                 LIMIT ?3",
            )
            .bind(source_entity_id)
            .bind(&like_pattern)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(edge_from_row).collect())
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
        self.bfs_core(start_entity_id, max_hops, None).await
    }

    /// BFS traversal considering only edges that were valid at `timestamp`.
    ///
    /// Equivalent to [`bfs_with_depth`] but replaces the `valid_to IS NULL` filter with
    /// the temporal range predicate `valid_from <= ts AND (valid_to IS NULL OR valid_to > ts)`.
    ///
    /// `timestamp` must be a `SQLite` datetime string: `"YYYY-MM-DD HH:MM:SS"`.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    pub async fn bfs_at_timestamp(
        &self,
        start_entity_id: i64,
        max_hops: u32,
        timestamp: &str,
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        self.bfs_core(start_entity_id, max_hops, Some(timestamp))
            .await
    }

    /// BFS traversal scoped to specific MAGMA edge types.
    ///
    /// When `edge_types` is empty, behaves identically to [`bfs_with_depth`] (traverses all
    /// active edges). When `edge_types` is non-empty, only traverses edges whose `edge_type`
    /// matches one of the provided types.
    ///
    /// This enables subgraph-scoped retrieval: a causal query traverses only causal + semantic
    /// edges, a temporal query only temporal + semantic edges, etc.
    ///
    /// Note: Semantic is typically included in `edge_types` by the caller to ensure recall is
    /// never worse than the untyped BFS. See `classify_graph_subgraph` in `router.rs`.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    pub async fn bfs_typed(
        &self,
        start_entity_id: i64,
        max_hops: u32,
        edge_types: &[EdgeType],
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        if edge_types.is_empty() {
            return self.bfs_with_depth(start_entity_id, max_hops).await;
        }
        self.bfs_core_typed(start_entity_id, max_hops, None, edge_types)
            .await
    }

    /// Shared BFS implementation.
    ///
    /// When `at_timestamp` is `None`, only active edges (`valid_to IS NULL`) are traversed.
    /// When `at_timestamp` is `Some(ts)`, edges valid at `ts` are traversed (temporal BFS).
    ///
    /// All IDs used in dynamic SQL come from our own database — no user input reaches the
    /// format string, so there is no SQL injection risk.
    async fn bfs_core(
        &self,
        start_entity_id: i64,
        max_hops: u32,
        at_timestamp: Option<&str>,
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
            let edge_filter = if at_timestamp.is_some() {
                let ts_pos = frontier.len() * 3 + 1;
                format!("valid_from <= ?{ts_pos} AND (valid_to IS NULL OR valid_to > ?{ts_pos})")
            } else {
                "valid_to IS NULL".to_owned()
            };
            let neighbour_sql = format!(
                "SELECT DISTINCT CASE
                     WHEN source_entity_id IN ({placeholders}) THEN target_entity_id
                     ELSE source_entity_id
                 END as neighbour_id
                 FROM graph_edges
                 WHERE {edge_filter}
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
            if let Some(ts) = at_timestamp {
                q = q.bind(ts);
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

        self.bfs_fetch_results(depth_map, at_timestamp).await
    }

    /// BFS implementation scoped to specific edge types.
    ///
    /// Builds the IN clause for `edge_type` filtering dynamically from enum values.
    /// All enum-derived strings come from `EdgeType::as_str()` — no user input reaches SQL.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    async fn bfs_core_typed(
        &self,
        start_entity_id: i64,
        max_hops: u32,
        at_timestamp: Option<&str>,
        edge_types: &[EdgeType],
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        use std::collections::HashMap;

        const MAX_FRONTIER: usize = 300;

        let type_strs: Vec<&str> = edge_types.iter().map(|t| t.as_str()).collect();

        let mut depth_map: HashMap<i64, u32> = HashMap::new();
        let mut frontier: Vec<i64> = vec![start_entity_id];
        depth_map.insert(start_entity_id, 0);

        let n_types = type_strs.len();
        // type_in is constant for the entire BFS — positions ?1..?n_types never change.
        let type_in = (1..=n_types)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let id_start = n_types + 1;

        for hop in 0..max_hops {
            if frontier.is_empty() {
                break;
            }
            frontier.truncate(MAX_FRONTIER);

            let n_frontier = frontier.len();
            // Positions: types first (?1..?n_types), then 3 copies of frontier IDs
            let frontier_placeholders = frontier
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", id_start + i))
                .collect::<Vec<_>>()
                .join(", ");

            let edge_filter = if at_timestamp.is_some() {
                let ts_pos = id_start + n_frontier * 3;
                format!(
                    "edge_type IN ({type_in}) AND valid_from <= ?{ts_pos} AND (valid_to IS NULL OR valid_to > ?{ts_pos})"
                )
            } else {
                format!("edge_type IN ({type_in}) AND valid_to IS NULL")
            };

            let neighbour_sql = format!(
                "SELECT DISTINCT CASE
                     WHEN source_entity_id IN ({frontier_placeholders}) THEN target_entity_id
                     ELSE source_entity_id
                 END as neighbour_id
                 FROM graph_edges
                 WHERE {edge_filter}
                   AND (source_entity_id IN ({frontier_placeholders}) OR target_entity_id IN ({frontier_placeholders}))"
            );

            let mut q = sqlx::query_scalar::<_, i64>(&neighbour_sql);
            // Bind types first
            for t in &type_strs {
                q = q.bind(*t);
            }
            // Bind frontier 3 times
            for id in &frontier {
                q = q.bind(*id);
            }
            for id in &frontier {
                q = q.bind(*id);
            }
            for id in &frontier {
                q = q.bind(*id);
            }
            if let Some(ts) = at_timestamp {
                q = q.bind(ts);
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

        // Fetch results — pass edge_type filter to bfs_fetch_results_typed
        self.bfs_fetch_results_typed(depth_map, at_timestamp, &type_strs)
            .await
    }

    /// Fetch entities and typed edges for a completed BFS depth map.
    ///
    /// Filters returned edges by the provided `edge_type` strings.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    async fn bfs_fetch_results_typed(
        &self,
        depth_map: std::collections::HashMap<i64, u32>,
        at_timestamp: Option<&str>,
        type_strs: &[&str],
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        let mut visited_ids: Vec<i64> = depth_map.keys().copied().collect();
        if visited_ids.is_empty() {
            return Ok((Vec::new(), Vec::new(), depth_map));
        }
        if visited_ids.len() > 499 {
            tracing::warn!(
                total = visited_ids.len(),
                retained = 499,
                "bfs_fetch_results_typed: visited entity set truncated to 499"
            );
            visited_ids.truncate(499);
        }

        let n_types = type_strs.len();
        let n_visited = visited_ids.len();

        // Bind order: types first, then visited_ids twice, then optional timestamp
        let type_in = (1..=n_types)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let id_start = n_types + 1;
        let placeholders = visited_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", id_start + i))
            .collect::<Vec<_>>()
            .join(", ");

        let edge_filter = if at_timestamp.is_some() {
            let ts_pos = id_start + n_visited * 2;
            format!(
                "edge_type IN ({type_in}) AND valid_from <= ?{ts_pos} AND (valid_to IS NULL OR valid_to > ?{ts_pos})"
            )
        } else {
            format!("edge_type IN ({type_in}) AND valid_to IS NULL")
        };

        let edge_sql = format!(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE {edge_filter}
               AND source_entity_id IN ({placeholders})
               AND target_entity_id IN ({placeholders})"
        );
        let mut edge_query = sqlx::query_as::<_, EdgeRow>(&edge_sql);
        for t in type_strs {
            edge_query = edge_query.bind(*t);
        }
        for id in &visited_ids {
            edge_query = edge_query.bind(*id);
        }
        for id in &visited_ids {
            edge_query = edge_query.bind(*id);
        }
        if let Some(ts) = at_timestamp {
            edge_query = edge_query.bind(ts);
        }
        let edge_rows: Vec<EdgeRow> = edge_query.fetch_all(&self.pool).await?;

        // For entity query, use plain sequential bind positions (no type prefix offset)
        let entity_sql2 = {
            let ph = visited_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
                 FROM graph_entities WHERE id IN ({ph})"
            )
        };
        let mut entity_query = sqlx::query_as::<_, EntityRow>(&entity_sql2);
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

    /// Fetch entities and edges for a completed BFS depth map.
    async fn bfs_fetch_results(
        &self,
        depth_map: std::collections::HashMap<i64, u32>,
        at_timestamp: Option<&str>,
    ) -> Result<(Vec<Entity>, Vec<Edge>, std::collections::HashMap<i64, u32>), MemoryError> {
        let mut visited_ids: Vec<i64> = depth_map.keys().copied().collect();
        if visited_ids.is_empty() {
            return Ok((Vec::new(), Vec::new(), depth_map));
        }
        // Edge query binds visited_ids twice — cap at 499 to stay under SQLite 999 limit.
        if visited_ids.len() > 499 {
            tracing::warn!(
                total = visited_ids.len(),
                retained = 499,
                "bfs_fetch_results: visited entity set truncated to 499 to stay within SQLite bind limit; \
                 some reachable entities will be dropped from results"
            );
            visited_ids.truncate(499);
        }

        let placeholders = visited_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let edge_filter = if at_timestamp.is_some() {
            let ts_pos = visited_ids.len() * 2 + 1;
            format!("valid_from <= ?{ts_pos} AND (valid_to IS NULL OR valid_to > ?{ts_pos})")
        } else {
            "valid_to IS NULL".to_owned()
        };
        let edge_sql = format!(
            "SELECT id, source_entity_id, target_entity_id, relation, fact, confidence,
                    valid_from, valid_to, created_at, expired_at, episode_id, qdrant_point_id,
                    edge_type
             FROM graph_edges
             WHERE {edge_filter}
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
        if let Some(ts) = at_timestamp {
            edge_query = edge_query.bind(ts);
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

    // ── Backfill helpers ──────────────────────────────────────────────────────

    /// Find an entity by name only (no type filter).
    ///
    /// Uses a two-phase lookup to ensure exact name matches are always prioritised:
    /// 1. Exact case-insensitive match on `name` or `canonical_name`.
    /// 2. If no exact match found, falls back to FTS5 prefix search (see `find_entities_fuzzy`).
    ///
    /// This prevents FTS5 from returning a different entity whose *summary* mentions the
    /// searched name (e.g. searching "Alice" returning "Google" because Google's summary
    /// contains "Alice").
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_entity_by_name(&self, name: &str) -> Result<Vec<Entity>, MemoryError> {
        let rows: Vec<EntityRow> = sqlx::query_as(
            "SELECT id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id
             FROM graph_entities
             WHERE name = ?1 COLLATE NOCASE OR canonical_name = ?1 COLLATE NOCASE
             LIMIT 5",
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?;

        if !rows.is_empty() {
            return rows.into_iter().map(entity_from_row).collect();
        }

        self.find_entities_fuzzy(name, 5).await
    }

    /// Return up to `limit` messages that have not yet been processed by graph extraction.
    ///
    /// Reads the `graph_processed` column added by migration 021.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn unprocessed_messages_for_backfill(
        &self,
        limit: usize,
    ) -> Result<Vec<(crate::types::MessageId, String)>, MemoryError> {
        let limit = i64::try_from(limit)?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, content FROM messages
             WHERE graph_processed = 0
             ORDER BY id ASC
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, content)| (crate::types::MessageId(id), content))
            .collect())
    }

    /// Return the count of messages not yet processed by graph extraction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn unprocessed_message_count(&self) -> Result<i64, MemoryError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE graph_processed = 0")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// Mark a batch of messages as graph-processed.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn mark_messages_graph_processed(
        &self,
        ids: &[crate::types::MessageId],
    ) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE messages SET graph_processed = 1 WHERE id IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id.0);
        }
        query.execute(&self.pool).await?;
        Ok(())
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
    edge_type: String,
}

fn edge_from_row(row: EdgeRow) -> Edge {
    let edge_type = row
        .edge_type
        .parse::<EdgeType>()
        .unwrap_or(EdgeType::Semantic);
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
        edge_type,
    }
}

#[derive(sqlx::FromRow)]
struct CommunityRow {
    id: i64,
    name: String,
    summary: String,
    entity_ids: String,
    fingerprint: Option<String>,
    created_at: String,
    updated_at: String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
