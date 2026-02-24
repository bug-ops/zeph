// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::sqlite::SqliteStore;
use crate::types::ConversationId;

#[derive(Debug, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub version: u32,
    pub exported_at: String,
    pub conversations: Vec<ConversationSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub id: i64,
    pub messages: Vec<MessageSnapshot>,
    pub summaries: Vec<SummarySnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageSnapshot {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: String,
    pub parts_json: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SummarySnapshot {
    pub id: i64,
    pub conversation_id: i64,
    pub content: String,
    pub first_message_id: i64,
    pub last_message_id: i64,
    pub token_estimate: i64,
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub conversations_imported: usize,
    pub messages_imported: usize,
    pub summaries_imported: usize,
    pub skipped: usize,
}

/// Export all conversations, messages and summaries from `SQLite` into a snapshot.
///
/// # Errors
///
/// Returns an error if any database query fails.
pub async fn export_snapshot(sqlite: &SqliteStore) -> Result<MemorySnapshot, MemoryError> {
    let conv_ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM conversations ORDER BY id ASC")
        .fetch_all(sqlite.pool())
        .await?;

    let exported_at = chrono_now();
    let mut conversations = Vec::with_capacity(conv_ids.len());

    for (cid_raw,) in conv_ids {
        let cid = ConversationId(cid_raw);

        let msg_rows: Vec<(i64, String, String, String, i64)> = sqlx::query_as(
            "SELECT id, role, content, parts, \
             COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0) \
             FROM messages WHERE conversation_id = ? ORDER BY id ASC",
        )
        .bind(cid)
        .fetch_all(sqlite.pool())
        .await?;

        let messages = msg_rows
            .into_iter()
            .map(
                |(id, role, content, parts_json, created_at)| MessageSnapshot {
                    id,
                    conversation_id: cid_raw,
                    role,
                    content,
                    parts_json,
                    created_at,
                },
            )
            .collect();

        let sum_rows = sqlite.load_summaries(cid).await?;
        let summaries = sum_rows
            .into_iter()
            .map(
                |(
                    id,
                    conversation_id,
                    content,
                    first_message_id,
                    last_message_id,
                    token_estimate,
                )| {
                    SummarySnapshot {
                        id,
                        conversation_id: conversation_id.0,
                        content,
                        first_message_id: first_message_id.0,
                        last_message_id: last_message_id.0,
                        token_estimate,
                    }
                },
            )
            .collect();

        conversations.push(ConversationSnapshot {
            id: cid_raw,
            messages,
            summaries,
        });
    }

    Ok(MemorySnapshot {
        version: 1,
        exported_at,
        conversations,
    })
}

/// Import a snapshot into `SQLite`, skipping duplicate entries.
///
/// Returns stats about what was imported.
///
/// # Errors
///
/// Returns an error if any database operation fails.
pub async fn import_snapshot(
    sqlite: &SqliteStore,
    snapshot: MemorySnapshot,
) -> Result<ImportStats, MemoryError> {
    if snapshot.version != 1 {
        return Err(MemoryError::Snapshot(format!(
            "unsupported snapshot version {}: only version 1 is supported",
            snapshot.version
        )));
    }
    let mut stats = ImportStats::default();

    for conv in snapshot.conversations {
        let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM conversations WHERE id = ?")
            .bind(conv.id)
            .fetch_optional(sqlite.pool())
            .await?;

        if exists.is_none() {
            sqlx::query("INSERT INTO conversations (id) VALUES (?)")
                .bind(conv.id)
                .execute(sqlite.pool())
                .await?;
            stats.conversations_imported += 1;
        } else {
            stats.skipped += 1;
        }

        for msg in conv.messages {
            let result = sqlx::query(
                "INSERT OR IGNORE INTO messages (id, conversation_id, role, content, parts) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(msg.id)
            .bind(msg.conversation_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(&msg.parts_json)
            .execute(sqlite.pool())
            .await?;

            if result.rows_affected() > 0 {
                stats.messages_imported += 1;
            } else {
                stats.skipped += 1;
            }
        }

        for sum in conv.summaries {
            let result = sqlx::query(
                "INSERT OR IGNORE INTO summaries \
                 (id, conversation_id, content, first_message_id, last_message_id, token_estimate) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(sum.id)
            .bind(sum.conversation_id)
            .bind(&sum.content)
            .bind(sum.first_message_id)
            .bind(sum.last_message_id)
            .bind(sum.token_estimate)
            .execute(sqlite.pool())
            .await?;

            if result.rows_affected() > 0 {
                stats.summaries_imported += 1;
            } else {
                stats.skipped += 1;
            }
        }
    }

    Ok(stats)
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO-8601 approximation without chrono dependency
    let (year, month, day, hour, min, sec) = unix_to_parts(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn unix_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let sec = secs % 60;
    let total_mins = secs / 60;
    let min = total_mins % 60;
    let total_hours = total_mins / 60;
    let hour = total_hours % 24;
    let total_days = total_hours / 24;

    // Gregorian calendar calculation (civil date from days since Unix epoch)
    let adjusted = total_days + 719_468;
    let era = adjusted / 146_097;
    let doe = adjusted - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn export_empty_database() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let snapshot = export_snapshot(&store).await.unwrap();
        assert_eq!(snapshot.version, 1);
        assert!(snapshot.conversations.is_empty());
        assert!(!snapshot.exported_at.is_empty());
    }

    #[tokio::test]
    async fn export_import_roundtrip() {
        let src = SqliteStore::new(":memory:").await.unwrap();
        let cid = src.create_conversation().await.unwrap();
        src.save_message(cid, "user", "hello export").await.unwrap();
        src.save_message(cid, "assistant", "hi import")
            .await
            .unwrap();

        let snapshot = export_snapshot(&src).await.unwrap();
        assert_eq!(snapshot.conversations.len(), 1);
        assert_eq!(snapshot.conversations[0].messages.len(), 2);

        let dst = SqliteStore::new(":memory:").await.unwrap();
        let stats = import_snapshot(&dst, snapshot).await.unwrap();
        assert_eq!(stats.conversations_imported, 1);
        assert_eq!(stats.messages_imported, 2);
        assert_eq!(stats.skipped, 0);

        let history = dst.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "hello export");
        assert_eq!(history[1].content, "hi import");
    }

    #[tokio::test]
    async fn import_duplicate_skips() {
        let src = SqliteStore::new(":memory:").await.unwrap();
        let cid = src.create_conversation().await.unwrap();
        src.save_message(cid, "user", "msg").await.unwrap();

        let snapshot = export_snapshot(&src).await.unwrap();

        let dst = SqliteStore::new(":memory:").await.unwrap();
        let stats1 = import_snapshot(&dst, snapshot).await.unwrap();
        assert_eq!(stats1.messages_imported, 1);

        let snapshot2 = export_snapshot(&src).await.unwrap();
        let stats2 = import_snapshot(&dst, snapshot2).await.unwrap();
        assert_eq!(stats2.messages_imported, 0);
        assert!(stats2.skipped > 0);
    }

    #[tokio::test]
    async fn import_existing_conversation_increments_skipped_not_imported() {
        let src = SqliteStore::new(":memory:").await.unwrap();
        let cid = src.create_conversation().await.unwrap();
        src.save_message(cid, "user", "only message").await.unwrap();

        let snapshot = export_snapshot(&src).await.unwrap();

        // Import once — conversation is new.
        let dst = SqliteStore::new(":memory:").await.unwrap();
        let stats1 = import_snapshot(&dst, snapshot).await.unwrap();
        assert_eq!(stats1.conversations_imported, 1);
        assert_eq!(stats1.messages_imported, 1);

        // Import again with no new messages — conversation already exists, must be counted as skipped.
        let snapshot2 = export_snapshot(&src).await.unwrap();
        let stats2 = import_snapshot(&dst, snapshot2).await.unwrap();
        assert_eq!(
            stats2.conversations_imported, 0,
            "existing conversation must not be counted as imported"
        );
        // The conversation itself contributes one skipped, plus the duplicate message.
        assert!(
            stats2.skipped >= 1,
            "re-importing an existing conversation must increment skipped"
        );
    }

    #[tokio::test]
    async fn export_includes_summaries() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "a").await.unwrap();
        let m2 = store.save_message(cid, "assistant", "b").await.unwrap();
        store.save_summary(cid, "summary", m1, m2, 5).await.unwrap();

        let snapshot = export_snapshot(&store).await.unwrap();
        assert_eq!(snapshot.conversations[0].summaries.len(), 1);
        assert_eq!(snapshot.conversations[0].summaries[0].content, "summary");
    }

    #[test]
    fn chrono_now_not_empty() {
        let ts = chrono_now();
        assert!(ts.contains('T'));
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn import_corrupt_json_returns_error() {
        let result = serde_json::from_str::<MemorySnapshot>("not valid json at all {{{");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn import_unsupported_version_returns_error() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let snapshot = MemorySnapshot {
            version: 99,
            exported_at: "2026-01-01T00:00:00Z".into(),
            conversations: vec![],
        };
        let err = import_snapshot(&store, snapshot).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported snapshot version 99"));
    }

    #[tokio::test]
    async fn import_partial_overlap_adds_new_messages() {
        let src = SqliteStore::new(":memory:").await.unwrap();
        let cid = src.create_conversation().await.unwrap();
        src.save_message(cid, "user", "existing message")
            .await
            .unwrap();

        let snapshot1 = export_snapshot(&src).await.unwrap();

        let dst = SqliteStore::new(":memory:").await.unwrap();
        let stats1 = import_snapshot(&dst, snapshot1).await.unwrap();
        assert_eq!(stats1.messages_imported, 1);

        src.save_message(cid, "assistant", "new reply")
            .await
            .unwrap();
        let snapshot2 = export_snapshot(&src).await.unwrap();
        let stats2 = import_snapshot(&dst, snapshot2).await.unwrap();

        assert_eq!(
            stats2.messages_imported, 1,
            "only the new message should be imported"
        );
        // skipped includes the existing conversation (1) plus the duplicate message (1).
        assert_eq!(
            stats2.skipped, 2,
            "existing conversation and duplicate message should be skipped"
        );

        let history = dst.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "new reply");
    }
}
