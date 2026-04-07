// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::TryStreamExt as _;
#[allow(unused_imports)]
use zeph_common;
use zeph_db::ActiveDialect;
use zeph_db::fts::sanitize_fts_query;
#[allow(unused_imports)]
use zeph_db::{begin_write, sql};
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use super::SqliteStore;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

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

/// Map a legacy externally-tagged variant key to the `kind` value used by the current
/// internally-tagged schema.
fn legacy_key_to_kind(key: &str) -> Option<&'static str> {
    match key {
        "Text" => Some("text"),
        "ToolOutput" => Some("tool_output"),
        "Recall" => Some("recall"),
        "CodeContext" => Some("code_context"),
        "Summary" => Some("summary"),
        "CrossSession" => Some("cross_session"),
        "ToolUse" => Some("tool_use"),
        "ToolResult" => Some("tool_result"),
        "Image" => Some("image"),
        "ThinkingBlock" => Some("thinking_block"),
        "RedactedThinkingBlock" => Some("redacted_thinking_block"),
        "Compaction" => Some("compaction"),
        _ => None,
    }
}

/// Attempt to parse a JSON string written in the pre-v0.17.1 externally-tagged format.
///
/// Old format: `[{"Summary":{"text":"..."}}, ...]`
/// New format: `[{"kind":"summary","text":"..."}, ...]`
///
/// Returns `None` if the input does not look like the old format or if any element fails
/// to deserialize after conversion.
fn try_parse_legacy_parts(parts_json: &str) -> Option<Vec<MessagePart>> {
    let array: Vec<serde_json::Value> = serde_json::from_str(parts_json).ok()?;
    let mut result = Vec::with_capacity(array.len());
    for element in array {
        let obj = element.as_object()?;
        if obj.contains_key("kind") {
            return None;
        }
        if obj.len() != 1 {
            return None;
        }
        let (key, inner) = obj.iter().next()?;
        let kind = legacy_key_to_kind(key)?;
        let mut new_obj = match inner {
            serde_json::Value::Object(m) => m.clone(),
            // Image variant wraps a single object directly
            other => {
                let mut m = serde_json::Map::new();
                m.insert("data".to_string(), other.clone());
                m
            }
        };
        new_obj.insert(
            "kind".to_string(),
            serde_json::Value::String(kind.to_string()),
        );
        let part: MessagePart = serde_json::from_value(serde_json::Value::Object(new_obj)).ok()?;
        result.push(part);
    }
    Some(result)
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
            if let Some(parts) = try_parse_legacy_parts(parts_json) {
                let truncated = parts_json.chars().take(120).collect::<String>();
                tracing::warn!(
                    role = %role_str,
                    parts_json = %truncated,
                    "loaded legacy-format message parts via compat path"
                );
                return parts;
            }
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
        let row: (ConversationId,) = zeph_db::query_as(sql!(
            "INSERT INTO conversations DEFAULT VALUES RETURNING id"
        ))
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

    /// Save a message with an optional category tag.
    ///
    /// The `category` column is NULL when `None` — existing rows are unaffected.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message_with_category(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        category: Option<&str>,
    ) -> Result<MessageId, MemoryError> {
        let importance_score = crate::semantic::importance::compute_importance(content, role);
        let row: (MessageId,) = zeph_db::query_as(sql!(
            "INSERT INTO messages \
                 (conversation_id, role, content, parts, agent_visible, user_visible, \
                  importance_score, category) \
                 VALUES (?, ?, ?, '[]', 1, 1, ?, ?) RETURNING id"
        ))
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .bind(importance_score)
        .bind(category)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
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
        const MAX_BYTES: usize = 100 * 1024;

        // Truncate plain-text content only. `parts_json` is skipped because a
        // mid-byte cut produces invalid JSON that breaks downstream deserialization.
        let content_cow: std::borrow::Cow<'_, str> = if content.len() > MAX_BYTES {
            let boundary = content.floor_char_boundary(MAX_BYTES);
            tracing::debug!(
                original_bytes = content.len(),
                "save_message: content exceeds 100KB, truncating"
            );
            std::borrow::Cow::Owned(format!(
                "{}... [truncated, {} bytes total]",
                &content[..boundary],
                content.len()
            ))
        } else {
            std::borrow::Cow::Borrowed(content)
        };

        let importance_score = crate::semantic::importance::compute_importance(&content_cow, role);
        let row: (MessageId,) = zeph_db::query_as(
            sql!("INSERT INTO messages (conversation_id, role, content, parts, agent_visible, user_visible, importance_score) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id"),
        )
        .bind(conversation_id)
        .bind(role)
        .bind(content_cow.as_ref())
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
        let rows: Vec<(String, String, String, i64, i64, i64)> = zeph_db::query_as(sql!(
            "SELECT role, content, parts, agent_visible, user_visible, id FROM (\
                SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
                WHERE conversation_id = ? AND deleted_at IS NULL \
                ORDER BY id DESC \
                LIMIT ?\
             ) ORDER BY id ASC"
        ))
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(
                |(role_str, content, parts_json, agent_visible, user_visible, row_id)| {
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
                            db_id: Some(row_id),
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

        let rows: Vec<(String, String, String, i64, i64, i64)> = zeph_db::query_as(
            sql!("WITH recent AS (\
                SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
                WHERE conversation_id = ? \
                  AND deleted_at IS NULL \
                  AND (? IS NULL OR agent_visible = ?) \
                  AND (? IS NULL OR user_visible = ?) \
                ORDER BY id DESC \
                LIMIT ?\
             ) SELECT role, content, parts, agent_visible, user_visible, id FROM recent ORDER BY id ASC"),
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
                |(role_str, content, parts_json, agent_visible, user_visible, row_id)| {
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
                            db_id: Some(row_id),
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

        zeph_db::query(sql!(
            "UPDATE messages SET agent_visible = 0, compacted_at = ? \
             WHERE conversation_id = ? AND id >= ? AND id <= ?"
        ))
        .bind(&now)
        .bind(conversation_id)
        .bind(start_id)
        .bind(end_id)
        .execute(&mut *tx)
        .await?;

        // importance_score uses schema DEFAULT 0.5 (neutral); compaction summaries are not scored at write time.
        let row: (MessageId,) = zeph_db::query_as(sql!(
            "INSERT INTO messages \
             (conversation_id, role, content, parts, agent_visible, user_visible) \
             VALUES (?, ?, ?, '[]', 1, 0) RETURNING id"
        ))
        .bind(conversation_id)
        .bind(summary_role)
        .bind(summary_content)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(row.0)
    }

    /// Atomically hide `tool_use/tool_result` message pairs and insert summary messages.
    ///
    /// Within a single transaction:
    /// 1. Sets `agent_visible=0, compacted_at=<now>` for each ID in `hide_ids`.
    /// 2. Inserts each text in `summaries` as a new agent-only assistant message.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails.
    pub async fn apply_tool_pair_summaries(
        &self,
        conversation_id: ConversationId,
        hide_ids: &[i64],
        summaries: &[String],
    ) -> Result<(), MemoryError> {
        if hide_ids.is_empty() && summaries.is_empty() {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();

        let mut tx = self.pool.begin().await?;

        for &id in hide_ids {
            zeph_db::query(sql!(
                "UPDATE messages SET agent_visible = 0, compacted_at = ? WHERE id = ?"
            ))
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        for summary in summaries {
            let content = format!("[tool summary] {summary}");
            let parts = serde_json::to_string(&[MessagePart::Summary {
                text: summary.clone(),
            }])
            .unwrap_or_else(|_| "[]".to_string());
            zeph_db::query(sql!(
                "INSERT INTO messages \
                 (conversation_id, role, content, parts, agent_visible, user_visible) \
                 VALUES (?, 'assistant', ?, ?, 1, 0)"
            ))
            .bind(conversation_id)
            .bind(&content)
            .bind(&parts)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
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
        let rows: Vec<(MessageId,)> = zeph_db::query_as(
            sql!("SELECT id FROM messages WHERE conversation_id = ? AND deleted_at IS NULL ORDER BY id ASC LIMIT ?"),
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
        let row: Option<(ConversationId,)> = zeph_db::query_as(sql!(
            "SELECT id FROM conversations ORDER BY id DESC LIMIT 1"
        ))
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
        let row: Option<(String, String, String, i64, i64)> = zeph_db::query_as(
            sql!("SELECT role, content, parts, agent_visible, user_visible FROM messages WHERE id = ? AND deleted_at IS NULL"),
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
                        db_id: None,
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
        let mut q = zeph_db::query_as::<_, (MessageId, String, String, String)>(&query);
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

        let rows: Vec<(MessageId, ConversationId, String, String)> = zeph_db::query_as(sql!(
            "SELECT m.id, m.conversation_id, m.role, m.content \
             FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL AND m.deleted_at IS NULL \
             ORDER BY m.id ASC \
             LIMIT ?"
        ))
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Count messages that have no embedding yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_unembedded_messages(&self) -> Result<usize, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL AND m.deleted_at IS NULL"
        ))
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(row.0).unwrap_or(usize::MAX))
    }

    /// Stream message IDs and content for messages without embeddings, one row at a time.
    ///
    /// Executes the same query as [`Self::unembedded_message_ids`] but returns a streaming
    /// cursor instead of loading all rows into a `Vec`. The `SQLite` read transaction is held
    /// for the duration of iteration; callers must not write to `embeddings_metadata` while
    /// the stream is live (use a separate async task for writes).
    ///
    /// # Errors
    ///
    /// Yields a [`MemoryError`] if the query or row decoding fails.
    pub fn stream_unembedded_messages(
        &self,
        limit: i64,
    ) -> impl futures::Stream<Item = Result<(MessageId, ConversationId, String, String), MemoryError>> + '_
    {
        zeph_db::query_as(sql!(
            "SELECT m.id, m.conversation_id, m.role, m.content \
             FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL AND m.deleted_at IS NULL \
             ORDER BY m.id ASC \
             LIMIT ?"
        ))
        .bind(limit)
        .fetch(&self.pool)
        .map_err(MemoryError::from)
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
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND deleted_at IS NULL"
        ))
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
            zeph_db::query_as(
                sql!("SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND id > ? AND deleted_at IS NULL"),
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
        let safe_query = sanitize_fts_query(query);
        if safe_query.is_empty() {
            return Ok(Vec::new());
        }

        let rows: Vec<(MessageId, f64)> = if let Some(cid) = conversation_id {
            zeph_db::query_as(
                sql!("SELECT m.id, -rank AS score \
                 FROM messages_fts f \
                 JOIN messages m ON m.id = f.rowid \
                 WHERE messages_fts MATCH ? AND m.conversation_id = ? AND m.agent_visible = 1 AND m.deleted_at IS NULL \
                 ORDER BY rank \
                 LIMIT ?"),
            )
            .bind(&safe_query)
            .bind(cid)
            .bind(effective_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            zeph_db::query_as(sql!(
                "SELECT m.id, -rank AS score \
                 FROM messages_fts f \
                 JOIN messages m ON m.id = f.rowid \
                 WHERE messages_fts MATCH ? AND m.agent_visible = 1 AND m.deleted_at IS NULL \
                 ORDER BY rank \
                 LIMIT ?"
            ))
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
        let safe_query = sanitize_fts_query(query);
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

        let mut q = zeph_db::query_as::<_, (MessageId, f64)>(&sql).bind(&safe_query);
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

        let placeholders: String =
            zeph_db::rewrite_placeholders(&ids.iter().map(|_| "?").collect::<Vec<_>>().join(","));
        let epoch_expr = <ActiveDialect as zeph_db::dialect::Dialect>::epoch_from_col("created_at");
        let query = format!(
            "SELECT id, {epoch_expr} FROM messages WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let mut q = zeph_db::query_as::<_, (MessageId, i64)>(&query);
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

        let rows: Vec<(MessageId, String, String)> = zeph_db::query_as(sql!(
            "SELECT id, role, content FROM messages \
             WHERE conversation_id = ? AND id > ? AND deleted_at IS NULL \
             ORDER BY id ASC LIMIT ?"
        ))
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
        let rows: Vec<(MessageId, String, Option<String>, i64)> = zeph_db::query_as(sql!(
            "SELECT id, created_at, last_accessed, access_count \
             FROM messages WHERE deleted_at IS NULL"
        ))
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
            zeph_db::query(
                sql!("UPDATE messages SET deleted_at = CURRENT_TIMESTAMP WHERE id = ? AND deleted_at IS NULL"),
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
        let rows: Vec<(MessageId,)> = zeph_db::query_as(sql!(
            "SELECT id FROM messages WHERE deleted_at IS NOT NULL AND qdrant_cleaned = 0"
        ))
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
            zeph_db::query(sql!("UPDATE messages SET qdrant_cleaned = 1 WHERE id = ?"))
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
        let mut q = zeph_db::query_as::<_, (MessageId, f64)>(&query);
        for &id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    /// Increment `access_count` and set `last_accessed = CURRENT_TIMESTAMP` for the given IDs.
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
            "UPDATE messages SET access_count = access_count + 1, last_accessed = CURRENT_TIMESTAMP \
             WHERE id IN ({placeholders})"
        );
        let mut q = zeph_db::query(&query);
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
        let rows: Vec<(MessageId, ConversationId, String, i64, f64)> = zeph_db::query_as(sql!(
            "SELECT id, conversation_id, content, session_count, importance_score \
             FROM messages \
             WHERE tier = 'episodic' AND session_count >= ? AND deleted_at IS NULL \
             ORDER BY session_count DESC, importance_score DESC \
             LIMIT ?"
        ))
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
        let rows: Vec<(String, i64)> = zeph_db::query_as(sql!(
            "SELECT tier, COUNT(*) FROM messages \
             WHERE deleted_at IS NULL AND tier IN ('episodic', 'semantic') \
             GROUP BY tier"
        ))
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
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM messages WHERE tier = 'semantic' AND deleted_at IS NULL"
        ))
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

        // Acquire the write lock immediately (BEGIN IMMEDIATE) to avoid the DEFERRED read->write
        // upgrade race that causes SQLITE_BUSY when a concurrent writer holds the lock.
        let mut tx = begin_write(&self.pool).await?;

        // Insert the new semantic fact.
        let epoch_now = <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::EPOCH_NOW;
        let promote_insert_raw = format!(
            "INSERT INTO messages \
             (conversation_id, role, content, parts, agent_visible, user_visible, \
              tier, promotion_timestamp) \
             VALUES (?, 'assistant', ?, '[]', 1, 0, 'semantic', {epoch_now}) \
             RETURNING id"
        );
        let promote_insert_sql = zeph_db::rewrite_placeholders(&promote_insert_raw);
        let row: (MessageId,) = zeph_db::query_as(&promote_insert_sql)
            .bind(conversation_id)
            .bind(merged_content)
            .fetch_one(&mut *tx)
            .await?;

        let new_id = row.0;

        // Soft-delete originals and reset qdrant_cleaned so eviction sweep removes vectors.
        for &id in original_ids {
            zeph_db::query(sql!(
                "UPDATE messages \
                 SET deleted_at = CURRENT_TIMESTAMP, qdrant_cleaned = 0 \
                 WHERE id = ? AND deleted_at IS NULL"
            ))
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
        let epoch_now = <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::EPOCH_NOW;
        let manual_promote_raw = format!(
            "UPDATE messages \
             SET tier = 'semantic', promotion_timestamp = {epoch_now} \
             WHERE id = ? AND deleted_at IS NULL AND tier = 'episodic'"
        );
        let manual_promote_sql = zeph_db::rewrite_placeholders(&manual_promote_raw);
        let mut count = 0usize;
        for &id in ids {
            let result = zeph_db::query(&manual_promote_sql)
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
        zeph_db::query(sql!(
            "UPDATE messages SET session_count = session_count + 1 \
             WHERE conversation_id = ? AND tier = 'episodic' AND deleted_at IS NULL"
        ))
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
        let mut q = zeph_db::query_as::<_, (MessageId, String)>(&query);
        for &id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    /// Return all conversation IDs that have at least one non-consolidated original message.
    ///
    /// Used by the consolidation sweep to find conversations that need processing.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn conversations_with_unconsolidated_messages(
        &self,
    ) -> Result<Vec<ConversationId>, MemoryError> {
        let rows: Vec<(ConversationId,)> = zeph_db::query_as(sql!(
            "SELECT DISTINCT conversation_id FROM messages \
             WHERE consolidated = 0 AND deleted_at IS NULL"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Fetch a batch of non-consolidated original messages for the consolidation sweep.
    ///
    /// Returns `(id, content)` pairs for messages that have not yet been processed by the
    /// consolidation loop (`consolidated = 0`) and are not soft-deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_unconsolidated_messages(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> Result<Vec<(MessageId, String)>, MemoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(MessageId, String)> = zeph_db::query_as(sql!(
            "SELECT id, content FROM messages \
             WHERE conversation_id = ? \
               AND consolidated = 0 \
               AND deleted_at IS NULL \
             ORDER BY id ASC \
             LIMIT ?"
        ))
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Look up which consolidated entry (if any) covers the given original message.
    ///
    /// Returns the `consolidated_id` of the first consolidation product that lists `source_id`
    /// in its sources, or `None` if no consolidated entry covers it.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn find_consolidated_for_source(
        &self,
        source_id: MessageId,
    ) -> Result<Option<MessageId>, MemoryError> {
        let row: Option<(MessageId,)> = zeph_db::query_as(sql!(
            "SELECT consolidated_id FROM memory_consolidation_sources \
             WHERE source_id = ? \
             LIMIT 1"
        ))
        .bind(source_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Insert a consolidated message and record its source linkage in a single transaction.
    ///
    /// Atomically:
    /// 1. Inserts the consolidated message with `consolidated = 1` and the given confidence.
    /// 2. Inserts rows into `memory_consolidation_sources` for each source ID.
    /// 3. Marks each source message's `consolidated = 1` so future sweeps skip them.
    ///
    /// If `confidence < confidence_threshold` the operation is skipped and `false` is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails. The transaction is rolled back automatically
    /// on failure so no partial state is written.
    pub async fn apply_consolidation_merge(
        &self,
        conversation_id: ConversationId,
        role: &str,
        merged_content: &str,
        source_ids: &[MessageId],
        confidence: f32,
        confidence_threshold: f32,
    ) -> Result<bool, MemoryError> {
        if confidence < confidence_threshold {
            return Ok(false);
        }
        if source_ids.is_empty() {
            return Ok(false);
        }

        let mut tx = self.pool.begin().await?;

        let importance = crate::semantic::importance::compute_importance(merged_content, role);
        let row: (MessageId,) = zeph_db::query_as(sql!(
            "INSERT INTO messages \
               (conversation_id, role, content, parts, agent_visible, user_visible, \
                importance_score, consolidated, consolidation_confidence) \
             VALUES (?, ?, ?, '[]', 1, 1, ?, 1, ?) \
             RETURNING id"
        ))
        .bind(conversation_id)
        .bind(role)
        .bind(merged_content)
        .bind(importance)
        .bind(confidence)
        .fetch_one(&mut *tx)
        .await?;
        let consolidated_id = row.0;

        let consol_sql = format!(
            "{} INTO memory_consolidation_sources (consolidated_id, source_id) VALUES (?, ?){}",
            <ActiveDialect as zeph_db::dialect::Dialect>::INSERT_IGNORE,
            <ActiveDialect as zeph_db::dialect::Dialect>::CONFLICT_NOTHING,
        );
        for &source_id in source_ids {
            zeph_db::query(&consol_sql)
                .bind(consolidated_id)
                .bind(source_id)
                .execute(&mut *tx)
                .await?;

            // Mark original as consolidated so future sweeps skip it.
            zeph_db::query(sql!("UPDATE messages SET consolidated = 1 WHERE id = ?"))
                .bind(source_id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Update an existing consolidated message in-place with new content.
    ///
    /// Atomically:
    /// 1. Updates `content` and `consolidation_confidence` on `target_id`.
    /// 2. Inserts rows into `memory_consolidation_sources` linking `target_id` → each source.
    /// 3. Marks each source message's `consolidated = 1`.
    ///
    /// If `confidence < confidence_threshold` the operation is skipped and `false` is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    pub async fn apply_consolidation_update(
        &self,
        target_id: MessageId,
        new_content: &str,
        additional_source_ids: &[MessageId],
        confidence: f32,
        confidence_threshold: f32,
    ) -> Result<bool, MemoryError> {
        if confidence < confidence_threshold {
            return Ok(false);
        }

        let mut tx = self.pool.begin().await?;

        zeph_db::query(sql!(
            "UPDATE messages SET content = ?, consolidation_confidence = ?, consolidated = 1 WHERE id = ?"
        ))
        .bind(new_content)
        .bind(confidence)
        .bind(target_id)
        .execute(&mut *tx)
        .await?;

        let consol_sql = format!(
            "{} INTO memory_consolidation_sources (consolidated_id, source_id) VALUES (?, ?){}",
            <ActiveDialect as zeph_db::dialect::Dialect>::INSERT_IGNORE,
            <ActiveDialect as zeph_db::dialect::Dialect>::CONFLICT_NOTHING,
        );
        for &source_id in additional_source_ids {
            zeph_db::query(&consol_sql)
                .bind(target_id)
                .bind(source_id)
                .execute(&mut *tx)
                .await?;

            zeph_db::query(sql!("UPDATE messages SET consolidated = 1 WHERE id = ?"))
                .bind(source_id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    // ── Forgetting sweep helpers ───────────────────────────────────────────────

    /// Set `importance_score` for a single message by ID.
    ///
    /// Used in tests and by forgetting sweep helpers.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn set_importance_score(&self, id: MessageId, score: f64) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE messages SET importance_score = ? WHERE id = ? AND deleted_at IS NULL"
        ))
        .bind(score)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get `importance_score` for a single message by ID.
    ///
    /// Returns `None` if the message does not exist or is deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn get_importance_score(&self, id: MessageId) -> Result<Option<f64>, MemoryError> {
        let row: Option<(f64,)> = zeph_db::query_as(sql!(
            "SELECT importance_score FROM messages WHERE id = ? AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(s,)| s))
    }

    /// Increment `access_count` and update `last_accessed` for a batch of messages.
    ///
    /// Alias used in forgetting tests; forwards to `increment_access_counts`.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn batch_increment_access_count(&self, ids: &[MessageId]) -> Result<(), MemoryError> {
        self.increment_access_counts(ids).await
    }

    /// Mark a set of messages as consolidated (`consolidated = 1`).
    ///
    /// Used in tests to simulate the state after consolidation.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn mark_messages_consolidated(&self, ids: &[i64]) -> Result<(), MemoryError> {
        for &id in ids {
            zeph_db::query(sql!("UPDATE messages SET consolidated = 1 WHERE id = ?"))
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Execute the three-phase forgetting sweep (`SleepGate`) inside a single transaction.
    ///
    /// **Phase 1** — Downscale all active non-consolidated importance scores by `decay_rate`.
    /// **Phase 2** — Undo the decay for messages accessed within `replay_window_hours` or
    ///   with `access_count >= replay_min_access_count` (undo current sweep's decay only).
    /// **Phase 3** — Soft-delete messages below `forgetting_floor` that are not protected
    ///   by recent access (`protect_recent_hours`) or high access count
    ///   (`protect_min_access_count`). Uses `batch_size` as a row-count cap.
    ///
    /// All phases commit atomically: concurrent readers see either the pre-sweep or
    /// post-sweep state, never an intermediate.
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    pub async fn run_forgetting_sweep_tx(
        &self,
        config: &zeph_common::config::memory::ForgettingConfig,
    ) -> Result<crate::forgetting::ForgettingResult, MemoryError> {
        let mut tx = self.pool.begin().await?;

        let decay = f64::from(config.decay_rate);
        let floor = f64::from(config.forgetting_floor);
        let batch = i64::try_from(config.sweep_batch_size).unwrap_or(i64::MAX);
        let replay_hours = i64::from(config.replay_window_hours);
        let replay_min_access = i64::from(config.replay_min_access_count);
        let protect_hours = i64::from(config.protect_recent_hours);
        let protect_min_access = i64::from(config.protect_min_access_count);

        // Phase 1: downscale all active, non-consolidated messages (limited to batch_size).
        // We target a specific set of IDs to respect sweep_batch_size.
        let candidate_ids: Vec<(MessageId,)> = zeph_db::query_as(sql!(
            "SELECT id FROM messages \
             WHERE deleted_at IS NULL AND consolidated = 0 \
             ORDER BY importance_score ASC \
             LIMIT ?"
        ))
        .bind(batch)
        .fetch_all(&mut *tx)
        .await?;

        #[allow(clippy::cast_possible_truncation)]
        let downscaled = candidate_ids.len() as u32;

        if downscaled > 0 {
            let placeholders: String = candidate_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let downscale_sql = format!(
                "UPDATE messages SET importance_score = importance_score * (1.0 - {decay}) \
                 WHERE id IN ({placeholders})"
            );
            let mut q = zeph_db::query(&downscale_sql);
            for &(id,) in &candidate_ids {
                q = q.bind(id);
            }
            q.execute(&mut *tx).await?;
        }

        // Phase 2: selective replay — undo decay for recently-accessed messages.
        // Formula: score / (1 - decay_rate) restores the current sweep's downscaling.
        // Cap at 1.0 to avoid exceeding the maximum importance score.
        // Scoped to the Phase 1 batch only: messages not decayed this sweep must not
        // have their scores inflated by the inverse formula.
        let replayed = if downscaled > 0 {
            let replay_placeholders: String = candidate_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let replay_sql = format!(
                "UPDATE messages \
                 SET importance_score = MIN(1.0, importance_score / (1.0 - {decay})) \
                 WHERE id IN ({replay_placeholders}) \
                 AND (\
                     (last_accessed IS NOT NULL \
                      AND last_accessed >= datetime('now', '-' || ? || ' hours')) \
                     OR access_count >= ?\
                 )"
            );
            let mut rq = zeph_db::query(&replay_sql);
            for &(id,) in &candidate_ids {
                rq = rq.bind(id);
            }
            let replay_result = rq
                .bind(replay_hours)
                .bind(replay_min_access)
                .execute(&mut *tx)
                .await?;
            #[allow(clippy::cast_possible_truncation)]
            let n = replay_result.rows_affected() as u32;
            n
        } else {
            0
        };

        // Phase 3: targeted forgetting — soft-delete low-score unprotected messages.
        let prune_sql = format!(
            "UPDATE messages \
             SET deleted_at = CURRENT_TIMESTAMP \
             WHERE deleted_at IS NULL AND consolidated = 0 \
             AND importance_score < {floor} \
             AND (\
                 last_accessed IS NULL \
                 OR last_accessed < datetime('now', '-' || ? || ' hours')\
             ) \
             AND access_count < ?"
        );
        let prune_result = zeph_db::query(&prune_sql)
            .bind(protect_hours)
            .bind(protect_min_access)
            .execute(&mut *tx)
            .await?;
        #[allow(clippy::cast_possible_truncation)]
        let pruned = prune_result.rows_affected() as u32;

        tx.commit().await?;

        Ok(crate::forgetting::ForgettingResult {
            downscaled,
            replayed,
            pruned,
        })
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
