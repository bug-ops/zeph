// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::ConversationId;
use zeph_db::ActiveDialect;
#[allow(unused_imports)]
use zeph_db::sql;

pub struct AcpSessionEvent {
    pub event_type: String,
    pub payload: String,
    pub created_at: String,
}

pub struct AcpSessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

/// Snapshot of per-session config fields (#5373), taken on graceful `session/close` so a later
/// `session/resume` or `session/fork` can inherit these values instead of resetting to
/// configured defaults.
pub struct AcpSessionConfigSnapshot {
    pub current_model: String,
    pub temperature_preset: String,
    pub thinking_enabled: bool,
    pub auto_approve_level: String,
}

impl SqliteStore {
    /// Create a new ACP session record.
    ///
    /// `owner` stamps `owner_key` (#5868): the authenticated ACP client identity that may
    /// list/load this session. `None` leaves it unowned — used by non-ACP channels
    /// (CLI/TUI/Telegram via `zeph_session::SessionStore::create`, which does not call this
    /// method at all and so is unaffected either way; `None` here is for ACP callers that
    /// intentionally create an unowned row, e.g. none today, but kept for forward compat).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn create_acp_session(
        &self,
        session_id: &str,
        owner: Option<&str>,
    ) -> Result<(), MemoryError> {
        let sql = zeph_db::rewrite_placeholders(&format!(
            "{} INTO acp_sessions (id, owner_key) VALUES (?, ?){}",
            <ActiveDialect as zeph_db::dialect::Dialect>::INSERT_IGNORE,
            <ActiveDialect as zeph_db::dialect::Dialect>::CONFLICT_NOTHING,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .bind(owner)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a single ACP session event.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn save_acp_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO acp_session_events (session_id, event_type, payload) VALUES (?, ?, ?)"
        ))
        .bind(session_id)
        .bind(event_type)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all events for an ACP session in insertion order.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_acp_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<AcpSessionEvent>, MemoryError> {
        let rows = zeph_db::query_as::<_, (String, String, String)>(
            sql!("SELECT event_type, payload, created_at FROM acp_session_events WHERE session_id = ? ORDER BY id"),
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(event_type, payload, created_at)| AcpSessionEvent {
                event_type,
                payload,
                created_at,
            })
            .collect())
    }

    /// Delete an ACP session only if it exists; returns `true` when a row was deleted.
    ///
    /// Eliminates the separate exists-check + delete TOCTOU race by relying on a
    /// single DELETE statement and inspecting affected rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete_acp_session_checked(&self, session_id: &str) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!("DELETE FROM acp_sessions WHERE id = ?"))
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List ACP sessions ordered by last activity descending.
    ///
    /// Includes title, `updated_at`, and message count per session.
    /// Pass `limit = 0` for unlimited results.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn list_acp_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<AcpSessionInfo>, MemoryError> {
        // spec-068 §12.3 / D-2: `acp_sessions.event_count` (migration 106, kept current by
        // `SessionStore::update_seq` on every turn flush per INV-SP-1) replaces the subquery
        // against `acp_session_events`, which the P1 write cutover leaves permanently empty for
        // post-cutover sessions.
        // `created_at`/`updated_at` are `TIMESTAMPTZ` on Postgres (`TEXT` on SQLite); project
        // both through `Dialect::select_as_text` so they decode into the `String` fields below,
        // mirroring `agent_sessions.rs::list_agent_sessions`'s fix for the same mismatch.
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let updated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
        let (limit_clause, limit_bind) = zeph_db::limit_clause(limit as u64);
        let raw = format!(
            "SELECT s.id, s.title, s.{created_at_sel}, s.{updated_at_sel}, \
             s.event_count AS message_count \
             FROM acp_sessions s \
             ORDER BY s.updated_at DESC{limit_clause}"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let mut query = zeph_db::query_as::<_, (String, Option<String>, String, String, i64)>(
            sqlx::AssertSqlSafe(query_sql),
        );
        if let Some(lim) = limit_bind {
            query = query.bind(lim);
        }
        let rows = query.fetch_all(&self.pool).await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                    id,
                    title,
                    created_at,
                    updated_at,
                    message_count,
                },
            )
            .collect())
    }

    /// Fetch metadata for a single ACP session.
    ///
    /// Returns `None` if the session does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_acp_session_info(
        &self,
        session_id: &str,
    ) -> Result<Option<AcpSessionInfo>, MemoryError> {
        // spec-068 §12.3 / D-2: see `list_acp_sessions` — `event_count` replaces the emptied
        // `acp_session_events` subquery.
        // `created_at`/`updated_at` are `TIMESTAMPTZ` on Postgres — see `list_acp_sessions`.
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let updated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
        let raw = format!(
            "SELECT s.id, s.title, s.{created_at_sel}, s.{updated_at_sel}, \
             s.event_count AS message_count \
             FROM acp_sessions s \
             WHERE s.id = ?"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row = zeph_db::query_as::<_, (String, Option<String>, String, String, i64)>(
            sqlx::AssertSqlSafe(query_sql),
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                id,
                title,
                created_at,
                updated_at,
                message_count,
            },
        ))
    }

    /// Insert multiple events for a session inside a single transaction.
    ///
    /// Atomically writes all events or none. More efficient than individual inserts
    /// for bulk import use cases.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction or any insert fails.
    pub async fn import_acp_events(
        &self,
        session_id: &str,
        events: &[(&str, &str)],
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;
        for (event_type, payload) in events {
            zeph_db::query(sql!(
                "INSERT INTO acp_session_events (session_id, event_type, payload) VALUES (?, ?, ?)"
            ))
            .bind(session_id)
            .bind(event_type)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Update the title of an ACP session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn update_session_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!("UPDATE acp_sessions SET title = ? WHERE id = ?"))
            .bind(title)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update the title of an ACP session; returns `true` when the row was found and updated.
    ///
    /// Eliminates the separate exists-check + update TOCTOU race by relying on a
    /// single UPDATE statement and inspecting affected rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn update_session_title_checked(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!("UPDATE acp_sessions SET title = ? WHERE id = ?"))
            .bind(title)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist a snapshot of the session's current config fields (#5373).
    ///
    /// Called on graceful `session/close` so a later `session/resume` or `session/fork` of a
    /// session no longer resident in memory can inherit these values.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn save_session_config(
        &self,
        session_id: &str,
        snapshot: &AcpSessionConfigSnapshot,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE acp_sessions SET current_model = ?, temperature_preset = ?, \
             thinking_enabled = ?, auto_approve_level = ? WHERE id = ?"
        ))
        .bind(&snapshot.current_model)
        .bind(&snapshot.temperature_preset)
        .bind(snapshot.thinking_enabled)
        .bind(&snapshot.auto_approve_level)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the persisted config snapshot for a session, if one was saved (#5373).
    ///
    /// Returns `None` when the session has no snapshot yet — either it was never closed
    /// gracefully, or it predates the config-snapshot migration. Callers should fall back to
    /// configured defaults in that case.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_session_config(
        &self,
        session_id: &str,
    ) -> Result<Option<AcpSessionConfigSnapshot>, MemoryError> {
        let row = zeph_db::query_as::<
            _,
            (Option<String>, Option<String>, Option<bool>, Option<String>),
        >(sql!(
            "SELECT current_model, temperature_preset, thinking_enabled, auto_approve_level \
                 FROM acp_sessions WHERE id = ?"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(
            |(current_model, temperature_preset, thinking_enabled, auto_approve_level)| {
                Some(AcpSessionConfigSnapshot {
                    current_model: current_model?,
                    temperature_preset: temperature_preset?,
                    thinking_enabled: thinking_enabled?,
                    auto_approve_level: auto_approve_level?,
                })
            },
        ))
    }

    /// Check whether an ACP session record exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn acp_session_exists(&self, session_id: &str) -> Result<bool, MemoryError> {
        let count: i64 =
            zeph_db::query_scalar(sql!("SELECT COUNT(*) FROM acp_sessions WHERE id = ?"))
                .bind(session_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(count > 0)
    }

    /// List ACP sessions owned by `owner`, ordered by last activity descending (#5868).
    ///
    /// Unlike [`Self::list_acp_sessions`], this is strictly scoped: rows owned by another
    /// `owner_key` or left `NULL` (legacy/non-ACP rows) are never returned. Pass `limit = 0`
    /// for unlimited results.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn list_acp_sessions_for_owner(
        &self,
        limit: usize,
        owner: &str,
    ) -> Result<Vec<AcpSessionInfo>, MemoryError> {
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let updated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
        let (limit_clause, limit_bind) = zeph_db::limit_clause(limit as u64);
        let raw = format!(
            "SELECT s.id, s.title, s.{created_at_sel}, s.{updated_at_sel}, \
             s.event_count AS message_count \
             FROM acp_sessions s \
             WHERE s.owner_key = ? \
             ORDER BY s.updated_at DESC{limit_clause}"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let mut query = zeph_db::query_as::<_, (String, Option<String>, String, String, i64)>(
            sqlx::AssertSqlSafe(query_sql),
        )
        .bind(owner);
        if let Some(lim) = limit_bind {
            query = query.bind(lim);
        }
        let rows = query.fetch_all(&self.pool).await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                    id,
                    title,
                    created_at,
                    updated_at,
                    message_count,
                },
            )
            .collect())
    }

    /// Fetch metadata for a session, scoped to `owner` (#5868).
    ///
    /// Returns `None` both when the session does not exist and when it is owned by a
    /// different `owner_key` — the two cases are indistinguishable to the caller by design,
    /// to avoid leaking existence of another owner's session (mirrors
    /// [`Self::claim_acp_session_for_owner`]'s uniform not-found semantics). A `NULL`
    /// (legacy/non-ACP) `owner_key` is treated as accessible.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_acp_session_info_for_owner(
        &self,
        session_id: &str,
        owner: &str,
    ) -> Result<Option<AcpSessionInfo>, MemoryError> {
        let created_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let updated_at_sel =
            <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
        let raw = format!(
            "SELECT s.id, s.title, s.{created_at_sel}, s.{updated_at_sel}, \
             s.event_count AS message_count \
             FROM acp_sessions s \
             WHERE s.id = ? AND (s.owner_key = ? OR s.owner_key IS NULL)"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row = zeph_db::query_as::<_, (String, Option<String>, String, String, i64)>(
            sqlx::AssertSqlSafe(query_sql),
        )
        .bind(session_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                id,
                title,
                created_at,
                updated_at,
                message_count,
            },
        ))
    }

    /// Non-mutating accessibility gate: `true` iff `session_id` exists and is owned by
    /// `owner` or is unowned (`NULL`, legacy/non-ACP row) (#5868).
    ///
    /// Use for read-only access gates that must NOT silently claim a legacy row (e.g. the
    /// HTTP `session_messages_handler`). For load/resume/fork, prefer
    /// [`Self::claim_acp_session_for_owner`], which additionally claims unowned rows
    /// atomically.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn acp_session_accessible_for_owner(
        &self,
        session_id: &str,
        owner: &str,
    ) -> Result<bool, MemoryError> {
        let count: i64 = zeph_db::query_scalar(sql!(
            "SELECT COUNT(*) FROM acp_sessions WHERE id = ? AND (owner_key = ? OR owner_key IS NULL)"
        ))
        .bind(session_id)
        .bind(owner)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Atomically grant `owner` access to `session_id`, claiming it if currently unowned
    /// (`NULL`, legacy/non-ACP row) (#5868).
    ///
    /// A single `UPDATE ... RETURNING` statement — no read-then-claim race window. Returns
    /// `true` iff the row exists AND is accessible: either it was `NULL` and is now claimed
    /// for `owner`, or it was already owned by `owner` (a redundant, harmless no-op write).
    /// Returns `false` uniformly for "session does not exist" and "owned by a different
    /// owner" — callers must map both to the same not-found response so a foreign
    /// `owner_key` can never be distinguished from a missing session.
    ///
    /// Use for load/resume/fork existence checks. For read-only gates that must not claim,
    /// use [`Self::acp_session_accessible_for_owner`] instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn claim_acp_session_for_owner(
        &self,
        session_id: &str,
        owner: &str,
    ) -> Result<bool, MemoryError> {
        let row: Option<(String,)> = zeph_db::query_as(sql!(
            "UPDATE acp_sessions SET owner_key = ? \
             WHERE id = ? AND (owner_key IS NULL OR owner_key = ?) \
             RETURNING id"
        ))
        .bind(owner)
        .bind(session_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Delete an ACP session scoped to `owner`, returning `true` iff a row was deleted
    /// (#5868).
    ///
    /// Matches [`Self::delete_acp_session_checked`]'s TOCTOU-free single-statement shape,
    /// additionally restricted to rows owned by `owner` or unowned (`NULL`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete_acp_session_for_owner(
        &self,
        session_id: &str,
        owner: &str,
    ) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM acp_sessions WHERE id = ? AND (owner_key = ? OR owner_key IS NULL)"
        ))
        .bind(session_id)
        .bind(owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the title of a session scoped to `owner`, returning `true` iff a row was
    /// updated (#5868).
    ///
    /// Matches [`Self::update_session_title_checked`]'s TOCTOU-free single-statement shape,
    /// additionally restricted to rows owned by `owner` or unowned (`NULL`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn update_session_title_for_owner(
        &self,
        session_id: &str,
        title: &str,
        owner: &str,
    ) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!(
            "UPDATE acp_sessions SET title = ? \
             WHERE id = ? AND (owner_key = ? OR owner_key IS NULL)"
        ))
        .bind(title)
        .bind(session_id)
        .bind(owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Create a new ACP session record with an associated conversation.
    ///
    /// `owner` stamps `owner_key` (#5868) — see [`Self::create_acp_session`].
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn create_acp_session_with_conversation(
        &self,
        session_id: &str,
        conversation_id: ConversationId,
        owner: Option<&str>,
    ) -> Result<(), MemoryError> {
        let sql = zeph_db::rewrite_placeholders(&format!(
            "{} INTO acp_sessions (id, conversation_id, owner_key) VALUES (?, ?, ?){}",
            <ActiveDialect as zeph_db::dialect::Dialect>::INSERT_IGNORE,
            <ActiveDialect as zeph_db::dialect::Dialect>::CONFLICT_NOTHING,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .bind(conversation_id)
            .bind(owner)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get the conversation ID associated with an ACP session.
    ///
    /// Returns `None` if the session has no conversation mapping (legacy session).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_acp_session_conversation_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ConversationId>, MemoryError> {
        let row: Option<(Option<ConversationId>,)> = zeph_db::query_as(sql!(
            "SELECT conversation_id FROM acp_sessions WHERE id = ?"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(cid,)| cid))
    }

    /// Update the conversation mapping for an ACP session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn set_acp_session_conversation_id(
        &self,
        session_id: &str,
        conversation_id: ConversationId,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE acp_sessions SET conversation_id = ? WHERE id = ?"
        ))
        .bind(conversation_id)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Copy all messages from one conversation to another, preserving order.
    ///
    /// Summaries are intentionally NOT copied: their `first_message_id`/`last_message_id`
    /// reference message IDs from the source conversation which differ from the new IDs
    /// assigned to the copied messages, making the compaction cursor incorrect. The forked
    /// session inherits the full message history and builds its own compaction state from
    /// scratch. Other per-conversation state also excluded: embeddings (re-indexed on demand),
    /// deferred tool summaries (treated as fresh context budget).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn copy_conversation(
        &self,
        source: ConversationId,
        target: ConversationId,
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;

        // Copy messages in order. Only columns present across all migrations are included;
        // per-message auto-fields (id, created_at, last_accessed, access_count, qdrant_cleaned)
        // are excluded so they are generated fresh for the target conversation.
        zeph_db::query(sql!(
            "INSERT INTO messages \
                (conversation_id, role, content, parts, visibility, compacted_at, deleted_at) \
             SELECT ?, role, content, parts, visibility, compacted_at, deleted_at \
             FROM messages WHERE conversation_id = ? ORDER BY id"
        ))
        .bind(target)
        .bind(source)
        .execute(&mut *tx)
        .await?;

        // Summaries are NOT copied — their message ID boundaries reference the source
        // conversation and would corrupt the compaction cursor in the forked session.
        // The forked session builds compaction state from its own messages.

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new")
    }

    /// Bump `acp_sessions.event_count` and `updated_at`, mirroring the `UPDATE`
    /// `zeph_session::SessionStore::update_seq` issues in production (spec-068 §12.3 / D-2).
    /// `list_acp_sessions`/`get_acp_session_info` read `event_count`, not the legacy
    /// `acp_session_events` table that `save_acp_event` populates — tests asserting on
    /// `message_count` (or activity ordering, which depends on `updated_at`) must drive both
    /// through this column directly rather than the retired write path.
    async fn bump_event_count(store: &SqliteStore, session_id: &str, event_count: i64) {
        let stmt = zeph_db::rewrite_placeholders(&format!(
            "UPDATE acp_sessions SET event_count = ?, updated_at = {} WHERE id = ?",
            <ActiveDialect as zeph_db::dialect::Dialect>::NOW,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(stmt))
            .bind(event_count)
            .bind(session_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_and_exists() {
        let store = make_store().await;
        store.create_acp_session("sess-1", None).await.unwrap();
        assert!(store.acp_session_exists("sess-1").await.unwrap());
        assert!(!store.acp_session_exists("sess-2").await.unwrap());
    }

    #[tokio::test]
    async fn session_config_round_trips() {
        let store = make_store().await;
        store.create_acp_session("sess-1", None).await.unwrap();
        let snapshot = AcpSessionConfigSnapshot {
            current_model: "claude:opus".to_owned(),
            temperature_preset: "creative".to_owned(),
            thinking_enabled: true,
            auto_approve_level: "auto-edit".to_owned(),
        };
        store
            .save_session_config("sess-1", &snapshot)
            .await
            .unwrap();

        let loaded = store
            .get_session_config("sess-1")
            .await
            .unwrap()
            .expect("snapshot must be present after save");
        assert_eq!(loaded.current_model, "claude:opus");
        assert_eq!(loaded.temperature_preset, "creative");
        assert!(loaded.thinking_enabled);
        assert_eq!(loaded.auto_approve_level, "auto-edit");
    }

    #[tokio::test]
    async fn session_config_missing_snapshot_returns_none() {
        let store = make_store().await;
        store.create_acp_session("sess-1", None).await.unwrap();
        assert!(store.get_session_config("sess-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_config_unknown_session_returns_none() {
        let store = make_store().await;
        assert!(store.get_session_config("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_and_load_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1", None).await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store
            .save_acp_event("sess-1", "agent_message", "world")
            .await
            .unwrap();

        let events = store.load_acp_events("sess-1").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "user_message");
        assert_eq!(events[0].payload, "hello");
        assert_eq!(events[1].event_type, "agent_message");
        assert_eq!(events[1].payload, "world");
    }

    #[tokio::test]
    async fn delete_cascades_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1", None).await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store.delete_acp_session_checked("sess-1").await.unwrap();

        assert!(!store.acp_session_exists("sess-1").await.unwrap());
        let events = store.load_acp_events("sess-1").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn load_events_empty_for_unknown() {
        let store = make_store().await;
        let events = store.load_acp_events("no-such").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_includes_title_and_message_count() {
        let store = make_store().await;
        store.create_acp_session("sess-b", None).await.unwrap();

        // Sleep so that sess-a's events land in a different second than sess-b's
        // created_at, making the updated_at DESC ordering deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        store.create_acp_session("sess-a", None).await.unwrap();
        bump_event_count(&store, "sess-a", 2).await;
        store
            .update_session_title("sess-a", "My Chat")
            .await
            .unwrap();

        let sessions = store.list_acp_sessions(100).await.unwrap();
        // sess-a has events so updated_at is newer — should be first
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].title.as_deref(), Some("My Chat"));
        assert_eq!(sessions[0].message_count, 2);

        // sess-b has no events
        let b = sessions.iter().find(|s| s.id == "sess-b").unwrap();
        assert!(b.title.is_none());
        assert_eq!(b.message_count, 0);
    }

    #[tokio::test]
    async fn list_sessions_respects_limit() {
        let store = make_store().await;
        for i in 0..5u8 {
            store
                .create_acp_session(&format!("sess-{i}"), None)
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(3).await.unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn list_sessions_limit_one_boundary() {
        let store = make_store().await;
        for i in 0..3u8 {
            store
                .create_acp_session(&format!("sess-{i}"), None)
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(1).await.unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[tokio::test]
    async fn list_sessions_unlimited_when_zero() {
        let store = make_store().await;
        for i in 0..5u8 {
            store
                .create_acp_session(&format!("sess-{i}"), None)
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(0).await.unwrap();
        assert_eq!(sessions.len(), 5);
    }

    #[tokio::test]
    async fn get_acp_session_info_returns_none_for_missing() {
        let store = make_store().await;
        let info = store.get_acp_session_info("no-such").await.unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn get_acp_session_info_returns_data() {
        let store = make_store().await;
        store.create_acp_session("sess-x", None).await.unwrap();
        bump_event_count(&store, "sess-x", 1).await;
        store.update_session_title("sess-x", "Test").await.unwrap();

        let info = store.get_acp_session_info("sess-x").await.unwrap().unwrap();
        assert_eq!(info.id, "sess-x");
        assert_eq!(info.title.as_deref(), Some("Test"));
        assert_eq!(info.message_count, 1);
    }

    #[tokio::test]
    async fn updated_at_trigger_fires_on_event_insert() {
        let store = make_store().await;
        store.create_acp_session("sess-t", None).await.unwrap();

        let before = store
            .get_acp_session_info("sess-t")
            .await
            .unwrap()
            .unwrap()
            .updated_at
            .clone();

        // Small sleep so datetime('now') differs
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        store
            .save_acp_event("sess-t", "user", "ping")
            .await
            .unwrap();

        let after = store
            .get_acp_session_info("sess-t")
            .await
            .unwrap()
            .unwrap()
            .updated_at;

        assert!(
            after > before,
            "updated_at should increase after event insert: before={before} after={after}"
        );
    }

    #[tokio::test]
    async fn create_session_with_conversation_and_retrieve() {
        let store = make_store().await;
        let cid = store.create_conversation().await.unwrap();
        store
            .create_acp_session_with_conversation("sess-1", cid, None)
            .await
            .unwrap();
        let retrieved = store
            .get_acp_session_conversation_id("sess-1")
            .await
            .unwrap();
        assert_eq!(retrieved, Some(cid));
    }

    #[tokio::test]
    async fn get_conversation_id_returns_none_for_legacy_session() {
        let store = make_store().await;
        store.create_acp_session("legacy", None).await.unwrap();
        let cid = store
            .get_acp_session_conversation_id("legacy")
            .await
            .unwrap();
        assert!(cid.is_none());
    }

    #[tokio::test]
    async fn get_conversation_id_returns_none_for_missing_session() {
        let store = make_store().await;
        let cid = store
            .get_acp_session_conversation_id("no-such")
            .await
            .unwrap();
        assert!(cid.is_none());
    }

    #[tokio::test]
    async fn set_conversation_id_updates_existing_session() {
        let store = make_store().await;
        store.create_acp_session("sess-2", None).await.unwrap();
        let cid = store.create_conversation().await.unwrap();
        store
            .set_acp_session_conversation_id("sess-2", cid)
            .await
            .unwrap();
        let retrieved = store
            .get_acp_session_conversation_id("sess-2")
            .await
            .unwrap();
        assert_eq!(retrieved, Some(cid));
    }

    #[tokio::test]
    async fn copy_conversation_copies_messages_in_order() {
        use zeph_llm::provider::Role;
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        store.save_message(src, "user", "hello").await.unwrap();
        store.save_message(src, "assistant", "world").await.unwrap();

        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();

        let msgs = store.load_history(dst, 100).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].content, "world");
    }

    #[tokio::test]
    async fn copy_conversation_empty_source_is_noop() {
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();
        let msgs = store.load_history(dst, 100).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn copy_conversation_does_not_copy_summaries() {
        // Summaries are intentionally excluded because their first/last_message_id
        // boundaries would reference source message IDs, corrupting the compaction cursor.
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        store.save_message(src, "user", "hello").await.unwrap();
        // Insert a summary directly so we can verify it is not copied.
        zeph_db::query(
            sql!("INSERT INTO summaries (conversation_id, content, first_message_id, last_message_id, token_estimate) \
             VALUES (?, 'summary text', 1, 1, 10)"),
        )
        .bind(src)
        .execute(&store.pool)
        .await
        .unwrap();

        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();

        let count: i64 = zeph_db::query_scalar(sql!(
            "SELECT COUNT(*) FROM summaries WHERE conversation_id = ?"
        ))
        .bind(dst)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            count, 0,
            "summaries must not be copied to forked conversation"
        );
    }

    #[tokio::test]
    async fn concurrent_sessions_get_distinct_conversation_ids() {
        let store = make_store().await;
        let cid1 = store.create_conversation().await.unwrap();
        let cid2 = store.create_conversation().await.unwrap();
        store
            .create_acp_session_with_conversation("sess-a", cid1, None)
            .await
            .unwrap();
        store
            .create_acp_session_with_conversation("sess-b", cid2, None)
            .await
            .unwrap();

        let retrieved1 = store
            .get_acp_session_conversation_id("sess-a")
            .await
            .unwrap();
        let retrieved2 = store
            .get_acp_session_conversation_id("sess-b")
            .await
            .unwrap();

        assert!(retrieved1.is_some());
        assert!(retrieved2.is_some());
        assert_ne!(
            retrieved1, retrieved2,
            "concurrent sessions must get distinct conversation_ids"
        );
    }

    // ── Owner-scoped access (#5868) ────────────────────────────────────────────

    /// Regression guard (#5868): the unscoped CLI-facing methods (`list_acp_sessions`,
    /// `acp_session_exists`, `delete_acp_session_checked`) must keep seeing every row
    /// regardless of `owner_key` — CLI/operator access is intentionally global, and a future
    /// change that accidentally scoped these instead of the `_for_owner` siblings would break
    /// `zeph sessions list`/`zeph sessions delete` silently.
    #[tokio::test]
    async fn cli_facing_unscoped_methods_see_all_owners_and_legacy_rows() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();
        store.create_acp_session("legacy", None).await.unwrap();

        let sessions = store.list_acp_sessions(0).await.unwrap();
        assert_eq!(sessions.len(), 3);

        assert!(store.acp_session_exists("mine").await.unwrap());
        assert!(store.acp_session_exists("theirs").await.unwrap());
        assert!(store.acp_session_exists("legacy").await.unwrap());

        assert!(store.delete_acp_session_checked("theirs").await.unwrap());
        assert!(!store.acp_session_exists("theirs").await.unwrap());
    }

    #[tokio::test]
    async fn list_for_owner_excludes_other_owners_and_legacy_rows() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();
        store.create_acp_session("legacy", None).await.unwrap();

        let sessions = store
            .list_acp_sessions_for_owner(0, "owner-a")
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "mine");
    }

    #[tokio::test]
    async fn get_info_for_owner_returns_none_for_foreign_owner() {
        let store = make_store().await;
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();
        let info = store
            .get_acp_session_info_for_owner("theirs", "owner-a")
            .await
            .unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn get_info_for_owner_accessible_for_legacy_null_row() {
        let store = make_store().await;
        store.create_acp_session("legacy", None).await.unwrap();
        let info = store
            .get_acp_session_info_for_owner("legacy", "owner-a")
            .await
            .unwrap();
        assert!(info.is_some());
    }

    #[tokio::test]
    async fn accessible_for_owner_true_for_own_and_legacy_false_for_foreign() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();
        store.create_acp_session("legacy", None).await.unwrap();
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();

        assert!(
            store
                .acp_session_accessible_for_owner("mine", "owner-a")
                .await
                .unwrap()
        );
        assert!(
            store
                .acp_session_accessible_for_owner("legacy", "owner-a")
                .await
                .unwrap()
        );
        assert!(
            !store
                .acp_session_accessible_for_owner("theirs", "owner-a")
                .await
                .unwrap()
        );
        assert!(
            !store
                .acp_session_accessible_for_owner("no-such", "owner-a")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn claim_for_owner_claims_legacy_null_row() {
        let store = make_store().await;
        store.create_acp_session("legacy", None).await.unwrap();

        assert!(
            store
                .claim_acp_session_for_owner("legacy", "owner-a")
                .await
                .unwrap()
        );

        // Now owned by owner-a: a foreign owner can no longer claim or list it.
        assert!(
            !store
                .claim_acp_session_for_owner("legacy", "owner-b")
                .await
                .unwrap()
        );
        let sessions = store
            .list_acp_sessions_for_owner(0, "owner-a")
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[tokio::test]
    async fn claim_for_owner_is_idempotent_for_the_same_owner() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();

        assert!(
            store
                .claim_acp_session_for_owner("mine", "owner-a")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn claim_for_owner_returns_false_for_missing_session() {
        let store = make_store().await;
        assert!(
            !store
                .claim_acp_session_for_owner("no-such", "owner-a")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn delete_for_owner_refuses_foreign_owner() {
        let store = make_store().await;
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();

        assert!(
            !store
                .delete_acp_session_for_owner("theirs", "owner-a")
                .await
                .unwrap()
        );
        assert!(store.acp_session_exists("theirs").await.unwrap());
    }

    #[tokio::test]
    async fn delete_for_owner_deletes_own_and_legacy_rows() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();
        store.create_acp_session("legacy", None).await.unwrap();

        assert!(
            store
                .delete_acp_session_for_owner("mine", "owner-a")
                .await
                .unwrap()
        );
        assert!(
            store
                .delete_acp_session_for_owner("legacy", "owner-a")
                .await
                .unwrap()
        );
        assert!(!store.acp_session_exists("mine").await.unwrap());
        assert!(!store.acp_session_exists("legacy").await.unwrap());
    }

    #[tokio::test]
    async fn update_title_for_owner_refuses_foreign_owner() {
        let store = make_store().await;
        store
            .create_acp_session("theirs", Some("owner-b"))
            .await
            .unwrap();

        assert!(
            !store
                .update_session_title_for_owner("theirs", "new title", "owner-a")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn update_title_for_owner_updates_own_row() {
        let store = make_store().await;
        store
            .create_acp_session("mine", Some("owner-a"))
            .await
            .unwrap();

        assert!(
            store
                .update_session_title_for_owner("mine", "new title", "owner-a")
                .await
                .unwrap()
        );
        let info = store
            .get_acp_session_info_for_owner("mine", "owner-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.title.as_deref(), Some("new title"));
    }
}
