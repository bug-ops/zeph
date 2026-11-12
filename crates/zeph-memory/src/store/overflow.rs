// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use uuid::Uuid;
use zeph_common::trust_level::{NativeToolKind, ToolResultSource, UntrustedTier};
#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::ConversationId;

/// `now() - N seconds` expression, dialect-selected at compile time.
///
/// `SQLite`'s `datetime('now', printf('-%d seconds', ?))` has no `PostgreSQL` equivalent —
/// `NOW() - INTERVAL '1 second' * ?` is the established substitution (mirrors the
/// `NOW() - INTERVAL '1 day' * ?` idiom already used in `store/skills.rs`).
fn now_minus_seconds_expr() -> &'static str {
    if cfg!(feature = "postgres") {
        "NOW() - INTERVAL '1 second' * ?"
    } else {
        "datetime('now', printf('-%d seconds', ?))"
    }
}

/// A stored tool result body together with the classification recorded when it was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowEntry {
    /// The stored body.
    pub content: Vec<u8>,
    /// How the body was classified when it was saved.
    pub source: ToolResultSource,
}

/// Column code for `result_source`; the exhaustive match makes a new variant a compile error.
fn encode_source(source: &ToolResultSource) -> (i16, Option<&str>) {
    match source {
        ToolResultSource::Native(NativeToolKind::Local) => (1, None),
        ToolResultSource::Native(NativeToolKind::Web) => (2, None),
        ToolResultSource::Native(NativeToolKind::Memory) => (3, None),
        ToolResultSource::Mcp { server_id } => (4, server_id.as_deref()),
        ToolResultSource::Archived(UntrustedTier::Local) => (5, None),
        ToolResultSource::Archived(UntrustedTier::External) => (6, None),
    }
}

/// Inverse of [`encode_source`]. A row recorded before the column existed (`NULL`) is treated as
/// MCP-served, the strictest classification; an unknown code is an error, never a default.
fn decode_source(
    code: Option<i16>,
    server_id: Option<String>,
) -> Result<ToolResultSource, MemoryError> {
    Ok(match code {
        None | Some(4) => ToolResultSource::Mcp { server_id },
        Some(1) => ToolResultSource::Native(NativeToolKind::Local),
        Some(2) => ToolResultSource::Native(NativeToolKind::Web),
        Some(3) => ToolResultSource::Native(NativeToolKind::Memory),
        Some(5) => ToolResultSource::Archived(UntrustedTier::Local),
        Some(6) => ToolResultSource::Archived(UntrustedTier::External),
        Some(other) => {
            return Err(MemoryError::InvalidInput(format!(
                "invalid tool_overflow result_source code: {other}"
            )));
        }
    })
}

impl SqliteStore {
    async fn insert_overflow_row(
        &self,
        conversation_id: ConversationId,
        content: &[u8],
        archive_type: &'static str,
        source: &ToolResultSource,
    ) -> Result<String, MemoryError> {
        let id = Uuid::new_v4().to_string();
        let byte_size = i64::try_from(content.len()).unwrap_or(i64::MAX);
        let (code, server_id) = encode_source(source);
        zeph_db::query(sql!(
            "INSERT INTO tool_overflow \
             (id, conversation_id, content, byte_size, archive_type, result_source, mcp_server_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&id)
        .bind(conversation_id)
        .bind(content)
        .bind(byte_size)
        .bind(archive_type)
        .bind(code)
        .bind(server_id)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Save overflow content with the classification of the tool result it came from,
    /// returning the generated UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn save_overflow(
        &self,
        conversation_id: ConversationId,
        content: &[u8],
        source: &ToolResultSource,
    ) -> Result<String, MemoryError> {
        self.insert_overflow_row(conversation_id, content, "overflow", source)
            .await
    }

    /// Save a Memex compaction-time archive, returning the generated UUID.
    ///
    /// Archives use `archive_type = 'archive'` and are excluded from the short-lived
    /// `cleanup_overflow()` job. They persist as long as the conversation exists. Archived text
    /// is post-sanitize context text, so only its untrusted `tier` is recorded.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn save_archive(
        &self,
        conversation_id: ConversationId,
        content: &[u8],
        tier: UntrustedTier,
    ) -> Result<String, MemoryError> {
        self.insert_overflow_row(
            conversation_id,
            content,
            "archive",
            &ToolResultSource::Archived(tier),
        )
        .await
    }

    /// Load overflow content and its recorded classification by UUID, scoped to the given
    /// conversation. Returns `None` if the entry does not exist or belongs to a different
    /// conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or the stored classification code is
    /// unknown.
    pub async fn load_overflow(
        &self,
        id: &str,
        conversation_id: ConversationId,
    ) -> Result<Option<OverflowEntry>, MemoryError> {
        let row: Option<(Vec<u8>, Option<i16>, Option<String>)> = zeph_db::query_as(sql!(
            "SELECT content, result_source, mcp_server_id FROM tool_overflow \
             WHERE id = ? AND conversation_id = ?"
        ))
        .bind(id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(content, code, server_id)| {
            Ok(OverflowEntry {
                content,
                source: decode_source(code, server_id)?,
            })
        })
        .transpose()
    }

    /// Delete execution-time overflow entries (`archive_type = 'overflow'`) older than
    /// `max_age_secs` seconds. Compaction-time archives (`archive_type = 'archive'`) are
    /// intentionally excluded — they persist for the lifetime of the conversation.
    ///
    /// Returns the number of deleted rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub async fn cleanup_overflow(&self, max_age_secs: u64) -> Result<u64, MemoryError> {
        let now_minus = now_minus_seconds_expr();
        let raw = format!(
            "DELETE FROM tool_overflow \
             WHERE archive_type = 'overflow' \
             AND created_at < {now_minus}"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let result = zeph_db::query(sqlx::AssertSqlSafe(query_sql))
            .bind(max_age_secs.cast_signed())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Return total overflow bytes stored for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn overflow_size(&self, conversation_id: ConversationId) -> Result<u64, MemoryError> {
        let total: Option<i64> = zeph_db::query_scalar(sql!(
            "SELECT COALESCE(SUM(byte_size), 0) FROM tool_overflow WHERE conversation_id = ?"
        ))
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.unwrap_or(0).cast_unsigned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the exact SQL fragment emitted by `now_minus_seconds_expr` for the dialect this
    /// test binary was compiled with. The `sqlite` and `postgres` features are additive on this
    /// crate (see `Cargo.toml`), so running with `--features postgres` exercises the branch that
    /// `cargo nextest run -p zeph-memory --lib` alone never reaches — a typo in the Postgres
    /// literal would otherwise only surface via a live-DB integration test or manual `psql` check.
    #[test]
    fn now_minus_seconds_expr_matches_active_dialect() {
        let expr = now_minus_seconds_expr();
        if cfg!(feature = "postgres") {
            assert_eq!(expr, "NOW() - INTERVAL '1 second' * ?");
        } else {
            assert_eq!(expr, "datetime('now', printf('-%d seconds', ?))");
        }
    }

    /// `now() - N days` literal, dialect-selected at compile time (see
    /// `now_minus_seconds_expr` above for the same idiom with a bound parameter).
    fn now_minus_days_literal(days: u32) -> String {
        if cfg!(feature = "postgres") {
            format!("NOW() - INTERVAL '{days} days'")
        } else {
            format!("datetime('now', '-{days} days')")
        }
    }

    const LOCAL: ToolResultSource = ToolResultSource::Native(NativeToolKind::Local);

    async fn make_store() -> (SqliteStore, ConversationId) {
        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
        (store, cid)
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let (store, cid) = make_store().await;
        let content = b"hello overflow world";
        let id = store
            .save_overflow(cid, content, &LOCAL)
            .await
            .expect("save");
        let loaded = store.load_overflow(&id, cid).await.expect("load");
        assert_eq!(
            loaded,
            Some(OverflowEntry {
                content: content.to_vec(),
                source: LOCAL
            })
        );
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let (store, cid) = make_store().await;
        let loaded = store
            .load_overflow("00000000-0000-0000-0000-000000000000", cid)
            .await
            .expect("load");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn load_wrong_conversation_returns_none() {
        let (store, cid1) = make_store().await;
        let cid2 = store
            .create_conversation()
            .await
            .expect("create_conversation");
        let id = store
            .save_overflow(cid1, b"secret", &LOCAL)
            .await
            .expect("save");
        // Loading with a different conversation_id must return None.
        let loaded = store.load_overflow(&id, cid2).await.expect("load");
        assert!(
            loaded.is_none(),
            "overflow entry must not be accessible from a different conversation"
        );
    }

    #[tokio::test]
    async fn overflow_size_empty_returns_zero() {
        let (store, cid) = make_store().await;
        let size = store.overflow_size(cid).await.expect("size");
        assert_eq!(size, 0);
    }

    #[tokio::test]
    async fn overflow_size_sums_byte_sizes() {
        let (store, cid) = make_store().await;
        store
            .save_overflow(cid, b"aaa", &LOCAL)
            .await
            .expect("save1");
        store
            .save_overflow(cid, b"bb", &LOCAL)
            .await
            .expect("save2");
        let size = store.overflow_size(cid).await.expect("size");
        assert_eq!(size, 5);
    }

    #[tokio::test]
    async fn cascade_delete_removes_overflow() {
        let (store, cid) = make_store().await;
        let id = store
            .save_overflow(cid, b"data", &LOCAL)
            .await
            .expect("save");
        // Delete the conversation — overflow should cascade.
        zeph_db::query(sql!("DELETE FROM conversations WHERE id = ?"))
            .bind(cid)
            .execute(store.pool())
            .await
            .expect("delete conversation");
        // Use a fresh store to load by id only — conversation is gone, use id=0 (will miss).
        // Verify via direct SQL that the row is gone.
        let count: i64 =
            zeph_db::query_scalar(sql!("SELECT COUNT(*) FROM tool_overflow WHERE id = ?"))
                .bind(&id)
                .fetch_one(store.pool())
                .await
                .expect("count");
        assert_eq!(count, 0, "overflow row should be removed by CASCADE");
    }

    #[tokio::test]
    async fn cleanup_removes_old_entries() {
        let (store, cid) = make_store().await;
        // Insert a row with an old timestamp.
        let id = Uuid::new_v4().to_string();
        let raw = format!(
            "INSERT INTO tool_overflow (id, conversation_id, content, byte_size, created_at) \
             VALUES (?, ?, ?, ?, {})",
            now_minus_days_literal(2)
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        zeph_db::query(sqlx::AssertSqlSafe(query_sql))
            .bind(&id)
            .bind(cid)
            .bind(b"old data".as_slice())
            .bind(8i64)
            .execute(store.pool())
            .await
            .expect("insert old row");

        // Insert a fresh row.
        let fresh_id = store
            .save_overflow(cid, b"fresh", &LOCAL)
            .await
            .expect("fresh");

        let deleted = store.cleanup_overflow(86400).await.expect("cleanup");
        assert_eq!(deleted, 1, "one old row should be deleted");

        assert!(
            store
                .load_overflow(&id, cid)
                .await
                .expect("load old")
                .is_none()
        );
        assert!(
            store
                .load_overflow(&fresh_id, cid)
                .await
                .expect("load fresh")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cleanup_fresh_entries_not_removed() {
        let (store, cid) = make_store().await;
        store.save_overflow(cid, b"a", &LOCAL).await.expect("save");
        store.save_overflow(cid, b"b", &LOCAL).await.expect("save");
        // Cleanup with 1 day retention — fresh entries should not be removed.
        let deleted = store.cleanup_overflow(86400).await.expect("cleanup");
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn save_archive_and_load_roundtrip() {
        let (store, cid) = make_store().await;
        let content = b"archived tool output body";
        let id = store
            .save_archive(cid, content, UntrustedTier::External)
            .await
            .expect("save_archive");
        // Archives are stored in the same table and loadable by the same API.
        let loaded = store.load_overflow(&id, cid).await.expect("load");
        assert_eq!(
            loaded,
            Some(OverflowEntry {
                content: content.to_vec(),
                source: ToolResultSource::Archived(UntrustedTier::External)
            })
        );
    }

    #[tokio::test]
    async fn cleanup_does_not_remove_old_archives() {
        let (store, cid) = make_store().await;
        // Insert a very old archive-type row directly.
        let archive_id = Uuid::new_v4().to_string();
        let archive_raw = format!(
            "INSERT INTO tool_overflow \
             (id, conversation_id, content, byte_size, archive_type, created_at) \
             VALUES (?, ?, ?, ?, 'archive', {})",
            now_minus_days_literal(30)
        );
        let archive_sql = zeph_db::rewrite_placeholders(&archive_raw);
        zeph_db::query(sqlx::AssertSqlSafe(archive_sql))
            .bind(&archive_id)
            .bind(cid)
            .bind(b"old archive".as_slice())
            .bind(11i64)
            .execute(store.pool())
            .await
            .expect("insert old archive");

        // Insert an old overflow-type row — this should be cleaned up.
        let overflow_id = Uuid::new_v4().to_string();
        let overflow_raw = format!(
            "INSERT INTO tool_overflow \
             (id, conversation_id, content, byte_size, archive_type, created_at) \
             VALUES (?, ?, ?, ?, 'overflow', {})",
            now_minus_days_literal(30)
        );
        let overflow_sql = zeph_db::rewrite_placeholders(&overflow_raw);
        zeph_db::query(sqlx::AssertSqlSafe(overflow_sql))
            .bind(&overflow_id)
            .bind(cid)
            .bind(b"old overflow".as_slice())
            .bind(12i64)
            .execute(store.pool())
            .await
            .expect("insert old overflow");

        let deleted = store.cleanup_overflow(86400).await.expect("cleanup");
        assert_eq!(deleted, 1, "only the overflow-type row should be deleted");

        // Archive must still be retrievable.
        assert!(
            store
                .load_overflow(&archive_id, cid)
                .await
                .expect("load archive")
                .is_some(),
            "archive must not be removed by cleanup"
        );
        // Overflow must be gone.
        assert!(
            store
                .load_overflow(&overflow_id, cid)
                .await
                .expect("load overflow")
                .is_none(),
            "old overflow must be removed by cleanup"
        );
    }

    fn every_source() -> Vec<ToolResultSource> {
        vec![
            ToolResultSource::Native(NativeToolKind::Local),
            ToolResultSource::Native(NativeToolKind::Web),
            ToolResultSource::Native(NativeToolKind::Memory),
            ToolResultSource::Mcp {
                server_id: Some("github".into()),
            },
            ToolResultSource::Mcp { server_id: None },
            ToolResultSource::Archived(UntrustedTier::Local),
            ToolResultSource::Archived(UntrustedTier::External),
        ]
    }

    #[tokio::test]
    async fn every_source_round_trips_through_the_store() {
        let (store, cid) = make_store().await;
        for source in every_source() {
            let id = store
                .save_overflow(cid, b"body", &source)
                .await
                .expect("save");
            let loaded = store.load_overflow(&id, cid).await.expect("load").unwrap();
            assert_eq!(loaded.source, source);
        }
    }

    #[tokio::test]
    async fn row_without_recorded_source_decodes_as_mcp() {
        let (store, cid) = make_store().await;
        let id = Uuid::new_v4().to_string();
        zeph_db::query(sql!(
            "INSERT INTO tool_overflow (id, conversation_id, content, byte_size) \
             VALUES (?, ?, ?, ?)"
        ))
        .bind(&id)
        .bind(cid)
        .bind(b"legacy".as_slice())
        .bind(6i64)
        .execute(store.pool())
        .await
        .expect("insert legacy row");
        let loaded = store.load_overflow(&id, cid).await.expect("load").unwrap();
        assert_eq!(loaded.source, ToolResultSource::Mcp { server_id: None });
    }

    #[test]
    fn unknown_source_code_is_an_error() {
        assert!(decode_source(Some(7), None).is_err());
        assert!(decode_source(Some(0), None).is_err());
        assert!(decode_source(Some(-1), None).is_err());
    }

    #[test]
    fn encode_decode_agree_for_every_source() {
        for source in every_source() {
            let (code, server) = encode_source(&source);
            let back = decode_source(Some(code), server.map(str::to_owned)).unwrap();
            assert_eq!(back, source);
        }
    }
}
