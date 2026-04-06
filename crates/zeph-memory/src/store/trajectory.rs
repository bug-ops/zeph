// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::{query, query_as, query_scalar, sql};

use super::DbStore;
use crate::error::MemoryError;
use crate::store::compression_guidelines::redact_sensitive;

/// Input for inserting a trajectory entry.
#[derive(Debug, Clone)]
pub struct NewTrajectoryEntry<'a> {
    pub conversation_id: Option<i64>,
    pub turn_index: i64,
    pub kind: &'a str,
    pub intent: &'a str,
    pub outcome: &'a str,
    pub tools_used: &'a str,
    pub confidence: f64,
}

/// A single trajectory memory row from the `trajectory_memory` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TrajectoryEntryRow {
    pub id: i64,
    pub conversation_id: Option<i64>,
    pub turn_index: i64,
    pub kind: String,
    pub intent: String,
    pub outcome: String,
    pub tools_used: String,
    pub confidence: f64,
    pub created_at: String,
    pub updated_at: String,
}

impl DbStore {
    /// Insert a trajectory entry.
    ///
    /// Returns the id of the inserted row.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn insert_trajectory_entry(
        &self,
        entry: NewTrajectoryEntry<'_>,
    ) -> Result<i64, MemoryError> {
        // Redact potential secrets echoed by the LLM from tool outputs before persisting.
        let intent = redact_sensitive(entry.intent);
        let outcome = redact_sensitive(entry.outcome);

        let (id,): (i64,) = query_as(sql!(
            "INSERT INTO trajectory_memory
                (conversation_id, turn_index, kind, intent, outcome, tools_used, confidence)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             RETURNING id"
        ))
        .bind(entry.conversation_id)
        .bind(entry.turn_index)
        .bind(entry.kind)
        .bind(intent.as_ref())
        .bind(outcome.as_ref())
        .bind(entry.tools_used)
        .bind(entry.confidence)
        .fetch_one(self.pool())
        .await?;

        Ok(id)
    }

    /// Load trajectory entries, optionally filtered by kind.
    ///
    /// Results are ordered by confidence DESC, then `created_at` DESC.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_trajectory_entries(
        &self,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TrajectoryEntryRow>, MemoryError> {
        let rows: Vec<TrajectoryEntryRow> = match kind {
            Some(k) => {
                query_as(sql!(
                    "SELECT id, conversation_id, turn_index, kind, intent, outcome,
                        tools_used, confidence, created_at, updated_at
                 FROM trajectory_memory
                 WHERE kind = ?
                 ORDER BY confidence DESC, created_at DESC
                 LIMIT ?"
                ))
                .bind(k)
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(self.pool())
                .await?
            }
            None => {
                query_as(sql!(
                    "SELECT id, conversation_id, turn_index, kind, intent, outcome,
                        tools_used, confidence, created_at, updated_at
                 FROM trajectory_memory
                 ORDER BY confidence DESC, created_at DESC
                 LIMIT ?"
                ))
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(self.pool())
                .await?
            }
        };

        Ok(rows)
    }

    /// Count total trajectory entries (for metrics/TUI).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_trajectory_entries(&self) -> Result<i64, MemoryError> {
        let count: i64 = query_scalar(sql!("SELECT COUNT(*) FROM trajectory_memory"))
            .fetch_one(self.pool())
            .await?;

        Ok(count)
    }

    /// Read the last extracted message id for a given conversation from `trajectory_meta`.
    ///
    /// Returns `0` if no row exists for the conversation yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn trajectory_last_extracted_message_id(
        &self,
        conversation_id: i64,
    ) -> Result<i64, MemoryError> {
        let id: Option<i64> = query_scalar(sql!(
            "SELECT last_extracted_message_id
             FROM trajectory_meta
             WHERE conversation_id = ?"
        ))
        .bind(conversation_id)
        .fetch_optional(self.pool())
        .await?;

        Ok(id.unwrap_or(0))
    }

    /// Upsert the last extracted message id for a given conversation in `trajectory_meta`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn set_trajectory_last_extracted_message_id(
        &self,
        conversation_id: i64,
        message_id: i64,
    ) -> Result<(), MemoryError> {
        query(sql!(
            "INSERT INTO trajectory_meta (conversation_id, last_extracted_message_id, updated_at)
             VALUES (?, ?, datetime('now'))
             ON CONFLICT(conversation_id) DO UPDATE SET
                 last_extracted_message_id = excluded.last_extracted_message_id,
                 updated_at = datetime('now')"
        ))
        .bind(conversation_id)
        .bind(message_id)
        .execute(self.pool())
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> DbStore {
        DbStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store")
    }

    #[tokio::test]
    async fn insert_trajectory_entry_basic() {
        let store = make_store().await;
        let id = store
            .insert_trajectory_entry(NewTrajectoryEntry {
                conversation_id: None,
                turn_index: 1,
                kind: "procedural",
                intent: "read a file",
                outcome: "file read successfully",
                tools_used: "[\"read_file\"]",
                confidence: 0.9,
            })
            .await
            .expect("insert");
        assert!(id > 0);
        assert_eq!(store.count_trajectory_entries().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn load_trajectory_entries_kind_filter() {
        let store = make_store().await;
        store
            .insert_trajectory_entry(NewTrajectoryEntry {
                conversation_id: None,
                turn_index: 1,
                kind: "procedural",
                intent: "build a crate",
                outcome: "built ok",
                tools_used: "[\"shell\"]",
                confidence: 0.8,
            })
            .await
            .expect("insert procedural");
        store
            .insert_trajectory_entry(NewTrajectoryEntry {
                conversation_id: None,
                turn_index: 2,
                kind: "episodic",
                intent: "fixed a bug",
                outcome: "patch applied",
                tools_used: "[\"shell\"]",
                confidence: 0.7,
            })
            .await
            .expect("insert episodic");

        let procedural = store
            .load_trajectory_entries(Some("procedural"), 10)
            .await
            .expect("load procedural");
        assert_eq!(procedural.len(), 1);
        assert_eq!(procedural[0].kind, "procedural");

        let all = store
            .load_trajectory_entries(None, 10)
            .await
            .expect("load all");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn trajectory_meta_per_conversation_tracking() {
        let store = make_store().await;
        // Two conversations — create real rows to satisfy FK.
        let cid1 = store.create_conversation().await.expect("create conv 1").0;
        let cid2 = store.create_conversation().await.expect("create conv 2").0;

        // Initial value is 0 for both.
        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid1)
                .await
                .expect("meta 1"),
            0
        );
        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid2)
                .await
                .expect("meta 2"),
            0
        );

        // Set for conv 1 — must not affect conv 2.
        store
            .set_trajectory_last_extracted_message_id(cid1, 42)
            .await
            .expect("set meta 1");

        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid1)
                .await
                .expect("meta 1 after"),
            42
        );
        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid2)
                .await
                .expect("meta 2 after"),
            0,
            "conv2 must remain 0 after conv1 update"
        );

        // Update conv 2 independently.
        store
            .set_trajectory_last_extracted_message_id(cid2, 99)
            .await
            .expect("set meta 2");
        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid2)
                .await
                .expect("meta 2 final"),
            99
        );
    }

    #[tokio::test]
    async fn trajectory_meta_upsert_idempotent() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("create conv").0;

        store
            .set_trajectory_last_extracted_message_id(cid, 10)
            .await
            .expect("first set");
        store
            .set_trajectory_last_extracted_message_id(cid, 20)
            .await
            .expect("second set");

        assert_eq!(
            store
                .trajectory_last_extracted_message_id(cid)
                .await
                .expect("final"),
            20
        );
    }
}
