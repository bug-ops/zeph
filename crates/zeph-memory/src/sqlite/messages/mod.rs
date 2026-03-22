// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use super::SqliteStore;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

/// Sanitize an arbitrary string into a valid FTS5 query.
///
/// Splits on non-alphanumeric characters, filters empty tokens, and joins
/// with spaces. This strips FTS5 special characters (`"`, `*`, `(`, `)`,
/// `^`, `-`, `+`, `:`) to prevent syntax errors in `MATCH` clauses.
///
/// Note: FTS5 boolean operators (AND, OR, NOT, NEAR) are preserved in their
/// original case. Callers that need to prevent operator interpretation must
/// filter these tokens separately (see `find_entities_fuzzy` in `graph/store.rs`).
pub(crate) fn sanitize_fts5_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_role(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    }
}

#[must_use]
pub fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Deserialize message parts from a stored JSON string.
///
/// Returns an empty `Vec` and logs a warning if deserialization fails, including the role and
/// a truncated excerpt of the malformed JSON for diagnostics.
fn parse_parts_json(role_str: &str, parts_json: &str) -> Vec<MessagePart> {
    if parts_json == "[]" {
        return vec![];
    }
    match serde_json::from_str(parts_json) {
        Ok(p) => p,
        Err(e) => {
            let truncated = parts_json.chars().take(120).collect::<String>();
            tracing::warn!(
                role = %role_str,
                parts_json = %truncated,
                error = %e,
                "failed to deserialize message parts, falling back to empty"
            );
            vec![]
        }
    }
}

impl SqliteStore {
    /// Create a new conversation and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn create_conversation(&self) -> Result<ConversationId, MemoryError> {
        let row: (ConversationId,) =
            sqlx::query_as("INSERT INTO conversations DEFAULT VALUES RETURNING id")
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    /// Save a message to the given conversation and return the message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
    ) -> Result<MessageId, MemoryError> {
        self.save_message_with_parts(conversation_id, role, content, "[]")
            .await
    }

    /// Save a message with structured parts JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message_with_parts(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
    ) -> Result<MessageId, MemoryError> {
        self.save_message_with_metadata(conversation_id, role, content, parts_json, true, true)
            .await
    }

    /// Save a message with visibility metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message_with_metadata(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
        agent_visible: bool,
        user_visible: bool,
    ) -> Result<MessageId, MemoryError> {
        let importance_score = crate::semantic::importance::compute_importance(content, role);
        let row: (MessageId,) = sqlx::query_as(
            "INSERT INTO messages (conversation_id, role, content, parts, agent_visible, user_visible, importance_score) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .bind(parts_json)
        .bind(i64::from(agent_visible))
        .bind(i64::from(user_visible))
        .bind(importance_score)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Load the most recent messages for a conversation, up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_history(
        &self,
        conversation_id: ConversationId,
        limit: u32,
    ) -> Result<Vec<Message>, MemoryError> {
        let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT role, content, parts, agent_visible, user_visible FROM (\
                SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
                WHERE conversation_id = ? AND deleted_at IS NULL \
                ORDER BY id DESC \
                LIMIT ?\
             ) ORDER BY id ASC",
        )
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(
                |(role_str, content, parts_json, agent_visible, user_visible)| {
                    let parts = parse_parts_json(&role_str, &parts_json);
                    Message {
                        role: parse_role(&role_str),
                        content,
                        parts,
                        metadata: MessageMetadata {
                            agent_visible: agent_visible != 0,
                            user_visible: user_visible != 0,
                            compacted_at: None,
                            deferred_summary: None,
                            focus_pinned: false,
                            focus_marker_id: None,
                        },
                    }
                },
            )
            .collect();
        Ok(messages)
    }

    /// Load messages filtered by visibility flags.
    ///
    /// Pass `Some(true)` to filter by a flag, `None` to skip filtering.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_history_filtered(
        &self,
        conversation_id: ConversationId,
        limit: u32,
        agent_visible: Option<bool>,
        user_visible: Option<bool>,
    ) -> Result<Vec<Message>, MemoryError> {
        let av = agent_visible.map(i64::from);
        let uv = user_visible.map(i64::from);

        let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
            "WITH recent AS (\
                SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
                WHERE conversation_id = ? \
                  AND deleted_at IS NULL \
                  AND (? IS NULL OR agent_visible = ?) \
                  AND (? IS NULL OR user_visible = ?) \
                ORDER BY id DESC \
                LIMIT ?\
             ) SELECT role, content, parts, agent_visible, user_visible FROM recent ORDER BY id ASC",
        )
        .bind(conversation_id)
        .bind(av)
        .bind(av)
        .bind(uv)
        .bind(uv)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(
                |(role_str, content, parts_json, agent_visible, user_visible)| {
                    let parts = parse_parts_json(&role_str, &parts_json);
                    Message {
                        role: parse_role(&role_str),
                        content,
                        parts,
                        metadata: MessageMetadata {
                            agent_visible: agent_visible != 0,
                            user_visible: user_visible != 0,
                            compacted_at: None,
                            deferred_summary: None,
                            focus_pinned: false,
                            focus_marker_id: None,
                        },
                    }
                },
            )
            .collect();
        Ok(messages)
    }

    /// Atomically mark a range of messages as user-only and insert a summary as agent-only.
    ///
    /// Within a single transaction:
    /// 1. Updates `agent_visible=0, compacted_at=now` for messages in `compacted_range`.
    /// 2. Inserts `summary_content` with `agent_visible=1, user_visible=0`.
    ///
    /// Returns the `MessageId` of the inserted summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails.
    pub async fn replace_conversation(
        &self,
        conversation_id: ConversationId,
        compacted_range: std::ops::RangeInclusive<MessageId>,
        summary_role: &str,
        summary_content: &str,
    ) -> Result<MessageId, MemoryError> {
        let now = {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("{secs}")
        };
        let start_id = compacted_range.start().0;
        let end_id = compacted_range.end().0;

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE messages SET agent_visible = 0, compacted_at = ? \
             WHERE conversation_id = ? AND id >= ? AND id <= ?",
        )
        .bind(&now)
        .bind(conversation_id)
        .bind(start_id)
        .bind(end_id)
        .execute(&mut *tx)
        .await?;

        // importance_score uses schema DEFAULT 0.5 (neutral); compaction summaries are not scored at write time.
        let row: (MessageId,) = sqlx::query_as(
            "INSERT INTO messages \
             (conversation_id, role, content, parts, agent_visible, user_visible) \
             VALUES (?, ?, ?, '[]', 1, 0) RETURNING id",
        )
        .bind(conversation_id)
        .bind(summary_role)
        .bind(summary_content)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(row.0)
    }

    /// Return the IDs of the N oldest messages in a conversation (ascending order).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn oldest_message_ids(
        &self,
        conversation_id: ConversationId,
        n: u32,
    ) -> Result<Vec<MessageId>, MemoryError> {
        let rows: Vec<(MessageId,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE conversation_id = ? AND deleted_at IS NULL ORDER BY id ASC LIMIT ?",
        )
        .bind(conversation_id)
        .bind(n)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Return the ID of the most recent conversation, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_conversation_id(&self) -> Result<Option<ConversationId>, MemoryError> {
        let row: Option<(ConversationId,)> =
            sqlx::query_as("SELECT id FROM conversations ORDER BY id DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }

    /// Fetch a single message by its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_by_id(
        &self,
        message_id: MessageId,
    ) -> Result<Option<Message>, MemoryError> {
        let row: Option<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT role, content, parts, agent_visible, user_visible FROM messages WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(role_str, content, parts_json, agent_visible, user_visible)| {
                let parts = parse_parts_json(&role_str, &parts_json);
                Message {
                    role: parse_role(&role_str),
                    content,
                    parts,
                    metadata: MessageMetadata {
                        agent_visible: agent_visible != 0,
                        user_visible: user_visible != 0,
                        compacted_at: None,
                        deferred_summary: None,
                        focus_pinned: false,
                        focus_marker_id: None,
                    },
                }
            },
        ))
    }

    /// Fetch messages by a list of IDs in a single query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn messages_by_ids(
        &self,
        ids: &[MessageId],
    ) -> Result<Vec<(MessageId, Message)>, MemoryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        let query = format!(
            "SELECT id, role, content, parts FROM messages \
             WHERE id IN ({placeholders}) AND agent_visible = 1 AND deleted_at IS NULL"
        );
        let mut q = sqlx::query_as::<_, (MessageId, String, String, String)>(&query);
        for &id in ids {
            q = q.bind(id);
        }

        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .into_iter()
            .map(|(id, role_str, content, parts_json)| {
                let parts = parse_parts_json(&role_str, &parts_json);
                (
                    id,
                    Message {
                        role: parse_role(&role_str),
                        content,
                        parts,
                        metadata: MessageMetadata::default(),
                    },
                )
            })
            .collect())
    }

    /// Return message IDs and content for messages without embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unembedded_message_ids(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<(MessageId, ConversationId, String, String)>, MemoryError> {
        let effective_limit = limit.map_or(i64::MAX, |l| i64::try_from(l).unwrap_or(i64::MAX));

        let rows: Vec<(MessageId, ConversationId, String, String)> = sqlx::query_as(
            "SELECT m.id, m.conversation_id, m.role, m.content \
             FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL AND m.deleted_at IS NULL \
             ORDER BY m.id ASC \
             LIMIT ?",
        )
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Count the number of messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages(
        &self,
        conversation_id: ConversationId,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND deleted_at IS NULL",
        )
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Count messages in a conversation with id greater than `after_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages_after(
        &self,
        conversation_id: ConversationId,
        after_id: MessageId,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) =
            sqlx::query_as(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND id > ? AND deleted_at IS NULL",
            )
            .bind(conversation_id)
            .bind(after_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Full-text keyword search over messages using FTS5.
    ///
    /// Returns message IDs with BM25 relevance scores (lower = more relevant,
    /// negated to positive for consistency with vector scores).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn keyword_search(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
    ) -> Result<Vec<(MessageId, f64)>, MemoryError> {
        let effective_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let safe_query = sanitize_fts5_query(query);
        if safe_query.is_empty() {
            return Ok(Vec::new());
        }

        let rows: Vec<(MessageId, f64)> = if let Some(cid) = conversation_id {
            sqlx::query_as(
                "SELECT m.id, -rank AS score \
                 FROM messages_fts f \
                 JOIN messages m ON m.id = f.rowid \
                 WHERE messages_fts MATCH ? AND m.conversation_id = ? AND m.agent_visible = 1 AND m.deleted_at IS NULL \
                 ORDER BY rank \
                 LIMIT ?",
            )
            .bind(&safe_query)
            .bind(cid)
            .bind(effective_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT m.id, -rank AS score \
                 FROM messages_fts f \
                 JOIN messages m ON m.id = f.rowid \
                 WHERE messages_fts MATCH ? AND m.agent_visible = 1 AND m.deleted_at IS NULL \
                 ORDER BY rank \
                 LIMIT ?",
            )
            .bind(&safe_query)
            .bind(effective_limit)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows)
    }

    /// Full-text keyword search over messages using FTS5, filtered by a `created_at` time range.
    ///
    /// Used by the `Episodic` recall path to combine keyword matching with temporal filtering.
    /// Temporal keywords are stripped from `query` by the caller before this method is invoked
    /// (see `strip_temporal_keywords`) to prevent BM25 score distortion.
    ///
    /// `after` and `before` are `SQLite` datetime strings in `YYYY-MM-DD HH:MM:SS` format (UTC).
    /// `None` means "no bound" on that side.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn keyword_search_with_time_range(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
        after: Option<&str>,
        before: Option<&str>,
    ) -> Result<Vec<(MessageId, f64)>, MemoryError> {
        let effective_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let safe_query = sanitize_fts5_query(query);
        if safe_query.is_empty() {
            return Ok(Vec::new());
        }

        // Build time-range clauses dynamically. Both bounds are optional.
        let after_clause = if after.is_some() {
            " AND m.created_at > ?"
        } else {
            ""
        };
        let before_clause = if before.is_some() {
            " AND m.created_at < ?"
        } else {
            ""
        };
        let conv_clause = if conversation_id.is_some() {
            " AND m.conversation_id = ?"
        } else {
            ""
        };

        let sql = format!(
            "SELECT m.id, -rank AS score \
             FROM messages_fts f \
             JOIN messages m ON m.id = f.rowid \
             WHERE messages_fts MATCH ? AND m.agent_visible = 1 AND m.deleted_at IS NULL\
             {after_clause}{before_clause}{conv_clause} \
             ORDER BY rank \
             LIMIT ?"
        );

        let mut q = sqlx::query_as::<_, (MessageId, f64)>(&sql).bind(&safe_query);
        if let Some(a) = after {
            q = q.bind(a);
        }
        if let Some(b) = before {
            q = q.bind(b);
        }
        if let Some(cid) = conversation_id {
            q = q.bind(cid);
        }
        q = q.bind(effective_limit);

        Ok(q.fetch_all(&self.pool).await?)
    }

    /// Fetch creation timestamps (Unix epoch seconds) for the given message IDs.
    ///
    /// Messages without a `created_at` column fall back to 0.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_timestamps(
        &self,
        ids: &[MessageId],
    ) -> Result<std::collections::HashMap<MessageId, i64>, MemoryError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0) \
             FROM messages WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let mut q = sqlx::query_as::<_, (MessageId, i64)>(&query);
        for &id in ids {
            q = q.bind(id);
        }

        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    /// Load a range of messages after a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_messages_range(
        &self,
        conversation_id: ConversationId,
        after_message_id: MessageId,
        limit: usize,
    ) -> Result<Vec<(MessageId, String, String)>, MemoryError> {
        let effective_limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let rows: Vec<(MessageId, String, String)> = sqlx::query_as(
            "SELECT id, role, content FROM messages \
             WHERE conversation_id = ? AND id > ? AND deleted_at IS NULL \
             ORDER BY id ASC LIMIT ?",
        )
        .bind(conversation_id)
        .bind(after_message_id)
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // ── Eviction helpers ──────────────────────────────────────────────────────

    /// Return all non-deleted message IDs with their eviction metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn get_eviction_candidates(
        &self,
    ) -> Result<Vec<crate::eviction::EvictionEntry>, crate::error::MemoryError> {
        let rows: Vec<(MessageId, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, created_at, last_accessed, access_count \
             FROM messages WHERE deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, created_at, last_accessed, access_count)| crate::eviction::EvictionEntry {
                    id,
                    created_at,
                    last_accessed,
                    access_count: access_count.try_into().unwrap_or(0),
                },
            )
            .collect())
    }

    /// Soft-delete a set of messages by marking `deleted_at`.
    ///
    /// Soft-deleted messages are excluded from all history queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn soft_delete_messages(
        &self,
        ids: &[MessageId],
    ) -> Result<(), crate::error::MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        // SQLite does not support array binding natively. Batch via individual updates.
        for &id in ids {
            sqlx::query(
                "UPDATE messages SET deleted_at = datetime('now') WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Return IDs of soft-deleted messages that have not yet been cleaned from Qdrant.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn get_soft_deleted_message_ids(
        &self,
    ) -> Result<Vec<MessageId>, crate::error::MemoryError> {
        let rows: Vec<(MessageId,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE deleted_at IS NOT NULL AND qdrant_cleaned = 0",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Mark a set of soft-deleted messages as Qdrant-cleaned.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn mark_qdrant_cleaned(
        &self,
        ids: &[MessageId],
    ) -> Result<(), crate::error::MemoryError> {
        for &id in ids {
            sqlx::query("UPDATE messages SET qdrant_cleaned = 1 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Fetch `importance_score` values for the given message IDs.
    ///
    /// Messages missing from the table fall back to 0.5 (neutral) and are omitted from the map.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn fetch_importance_scores(
        &self,
        ids: &[MessageId],
    ) -> Result<std::collections::HashMap<MessageId, f64>, MemoryError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, importance_score FROM messages WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let mut q = sqlx::query_as::<_, (MessageId, f64)>(&query);
        for &id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    /// Increment `access_count` and set `last_accessed = datetime('now')` for the given IDs.
    ///
    /// Skips the update when `ids` is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn increment_access_counts(&self, ids: &[MessageId]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "UPDATE messages SET access_count = access_count + 1, last_accessed = datetime('now') \
             WHERE id IN ({placeholders})"
        );
        let mut q = sqlx::query(&query);
        for &id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    // ── Tier promotion helpers ─────────────────────────────────────────────────

    /// Return episodic messages with `session_count >= min_sessions`, ordered by
    /// session count descending then importance score descending.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn find_promotion_candidates(
        &self,
        min_sessions: u32,
        batch_size: usize,
    ) -> Result<Vec<PromotionCandidate>, MemoryError> {
        let limit = i64::try_from(batch_size).unwrap_or(i64::MAX);
        let min = i64::from(min_sessions);
        let rows: Vec<(MessageId, ConversationId, String, i64, f64)> = sqlx::query_as(
            "SELECT id, conversation_id, content, session_count, importance_score \
             FROM messages \
             WHERE tier = 'episodic' AND session_count >= ? AND deleted_at IS NULL \
             ORDER BY session_count DESC, importance_score DESC \
             LIMIT ?",
        )
        .bind(min)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, conversation_id, content, session_count, importance_score)| {
                    PromotionCandidate {
                        id,
                        conversation_id,
                        content,
                        session_count: session_count.try_into().unwrap_or(0),
                        importance_score,
                    }
                },
            )
            .collect())
    }

    /// Count messages per tier (episodic, semantic) that are not deleted.
    ///
    /// Returns `(episodic_count, semantic_count)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages_by_tier(&self) -> Result<(i64, i64), MemoryError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT tier, COUNT(*) FROM messages \
             WHERE deleted_at IS NULL AND tier IN ('episodic', 'semantic') \
             GROUP BY tier",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut episodic = 0i64;
        let mut semantic = 0i64;
        for (tier, count) in rows {
            match tier.as_str() {
                "episodic" => episodic = count,
                "semantic" => semantic = count,
                _ => {}
            }
        }
        Ok((episodic, semantic))
    }

    /// Count semantic facts (tier='semantic', not deleted).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_semantic_facts(&self) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages WHERE tier = 'semantic' AND deleted_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Promote a set of episodic messages to semantic tier in a single transaction.
    ///
    /// Within one transaction:
    /// 1. Inserts a new message with `tier='semantic'` and `promotion_timestamp=unixepoch()`.
    /// 2. Soft-deletes the original episodic messages and marks them `qdrant_cleaned=0`
    ///    so the eviction sweep picks up their Qdrant vectors.
    ///
    /// Returns the `MessageId` of the new semantic message.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails.
    pub async fn promote_to_semantic(
        &self,
        conversation_id: ConversationId,
        merged_content: &str,
        original_ids: &[MessageId],
    ) -> Result<MessageId, MemoryError> {
        if original_ids.is_empty() {
            return Err(MemoryError::Other(
                "promote_to_semantic: original_ids must not be empty".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;

        // Insert the new semantic fact.
        let row: (MessageId,) = sqlx::query_as(
            "INSERT INTO messages \
             (conversation_id, role, content, parts, agent_visible, user_visible, \
              tier, promotion_timestamp) \
             VALUES (?, 'assistant', ?, '[]', 1, 0, 'semantic', unixepoch()) \
             RETURNING id",
        )
        .bind(conversation_id)
        .bind(merged_content)
        .fetch_one(&mut *tx)
        .await?;

        let new_id = row.0;

        // Soft-delete originals and reset qdrant_cleaned so eviction sweep removes vectors.
        for &id in original_ids {
            sqlx::query(
                "UPDATE messages \
                 SET deleted_at = datetime('now'), qdrant_cleaned = 0 \
                 WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(new_id)
    }

    /// Manually promote a set of messages to semantic tier without merging.
    ///
    /// Sets `tier='semantic'` and `promotion_timestamp=unixepoch()` for the given IDs.
    /// Does NOT soft-delete the originals — use this for direct user-requested promotion.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn manual_promote(&self, ids: &[MessageId]) -> Result<usize, MemoryError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut count = 0usize;
        for &id in ids {
            let result = sqlx::query(
                "UPDATE messages \
                 SET tier = 'semantic', promotion_timestamp = unixepoch() \
                 WHERE id = ? AND deleted_at IS NULL AND tier = 'episodic'",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
            count += usize::try_from(result.rows_affected()).unwrap_or(0);
        }
        Ok(count)
    }

    /// Increment `session_count` for all episodic messages in a conversation.
    ///
    /// Called when a session restores an existing conversation to mark that messages
    /// were accessed in a new session. Only episodic (non-deleted) messages are updated.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn increment_session_counts_for_conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "UPDATE messages SET session_count = session_count + 1 \
             WHERE conversation_id = ? AND tier = 'episodic' AND deleted_at IS NULL",
        )
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the tier string for each of the given message IDs.
    ///
    /// Messages not found or already deleted are omitted from the result.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn fetch_tiers(
        &self,
        ids: &[MessageId],
    ) -> Result<std::collections::HashMap<MessageId, String>, MemoryError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, tier FROM messages WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let mut q = sqlx::query_as::<_, (MessageId, String)>(&query);
        for &id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }
}

/// A candidate message for tier promotion, returned by [`SqliteStore::find_promotion_candidates`].
#[derive(Debug, Clone)]
pub struct PromotionCandidate {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub content: String,
    pub session_count: u32,
    pub importance_score: f64,
}

#[cfg(test)]
mod tests;
