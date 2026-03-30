// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed store for RL admission control training data (#2416).
//!
//! Records ALL messages seen by A-MAC (admitted and rejected) to avoid survivorship
//! bias in the logistic regression model (critic fix C3). `was_recalled` is set to 1
//! when `SemanticMemory::recall()` returns the message, providing positive training signal.

#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::{ConversationId, MessageId};

/// Input for recording a single RL admission training sample.
pub struct AdmissionTrainingInput<'a> {
    pub message_id: Option<MessageId>,
    pub conversation_id: ConversationId,
    pub content: &'a str,
    pub role: &'a str,
    pub composite_score: f32,
    pub was_admitted: bool,
    pub features_json: &'a str,
}

/// A single training record for the RL admission model.
#[derive(Debug, Clone)]
pub struct AdmissionTrainingRecord {
    pub id: i64,
    pub message_id: Option<i64>,
    pub conversation_id: ConversationId,
    pub content_hash: String,
    pub role: String,
    pub composite_score: f32,
    pub was_admitted: bool,
    pub was_recalled: bool,
    pub features_json: String,
    pub created_at: String,
}

/// Compute a stable 16-char hex hash of `content` for deduplication.
///
/// Uses the first 8 bytes of SHA-256 truncated to a 16-char hex string.
/// SHA-256 output is stable across Rust toolchain versions, unlike `DefaultHasher`.
#[must_use]
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    format!("{:016x}", u64::from_be_bytes(bytes))
}

impl SqliteStore {
    /// Record a message in the RL admission training data.
    ///
    /// Called for BOTH admitted and rejected messages so the model sees both classes.
    /// `message_id` is `None` for rejected messages (never persisted to `messages` table).
    /// `features_json` is the JSON-serialized feature vector used for training.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn record_admission_training(
        &self,
        input: AdmissionTrainingInput<'_>,
    ) -> Result<i64, MemoryError> {
        let hash = content_hash(input.content);
        let admitted_i = i64::from(input.was_admitted);
        let msg_id = input.message_id.map(|m| m.0);
        let (conversation_id, role, composite_score, features_json) = (
            input.conversation_id,
            input.role,
            input.composite_score,
            input.features_json,
        );
        let id = zeph_db::query_scalar(sql!(
            "INSERT INTO admission_training_data \
             (message_id, conversation_id, content_hash, role, composite_score, \
              was_admitted, was_recalled, features_json) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?) \
             RETURNING id"
        ))
        .bind(msg_id)
        .bind(conversation_id.0)
        .bind(hash)
        .bind(role)
        .bind(f64::from(composite_score))
        .bind(admitted_i)
        .bind(features_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Mark training records as recalled for the given message IDs.
    ///
    /// Called after `batch_increment_access_count()` in `SemanticMemory::recall()`.
    /// Sets `was_recalled = 1` and updates `updated_at` for all matching records.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn mark_training_recalled(
        &self,
        message_ids: &[MessageId],
    ) -> Result<(), MemoryError> {
        if message_ids.is_empty() {
            return Ok(());
        }
        let placeholders: String = message_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "UPDATE admission_training_data \
             SET was_recalled = 1, updated_at = datetime('now') \
             WHERE message_id IN ({placeholders})"
        );
        let mut q = zeph_db::query(&query);
        for id in message_ids {
            q = q.bind(id.0);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Count total training records (admitted + rejected).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn count_training_records(&self) -> Result<i64, MemoryError> {
        let count = zeph_db::query_scalar(sql!("SELECT COUNT(*) FROM admission_training_data"))
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Get a batch of training records for model training.
    ///
    /// Returns up to `limit` records ordered by creation time (oldest first).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_training_batch(
        &self,
        limit: usize,
    ) -> Result<Vec<AdmissionTrainingRecord>, MemoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = zeph_db::query_as::<
            _,
            (
                i64,
                Option<i64>,
                i64,
                String,
                String,
                f64,
                i64,
                i64,
                String,
                String,
            ),
        >(sql!(
            "SELECT id, message_id, conversation_id, content_hash, role, \
                    composite_score, was_admitted, was_recalled, features_json, created_at \
             FROM admission_training_data \
             ORDER BY created_at ASC \
             LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, msg_id, cid, hash, role, score, admitted, recalled, features, created_at)| {
                    AdmissionTrainingRecord {
                        id,
                        message_id: msg_id,
                        conversation_id: ConversationId(cid),
                        content_hash: hash,
                        role,
                        #[expect(clippy::cast_possible_truncation)]
                        composite_score: score as f32,
                        was_admitted: admitted != 0,
                        was_recalled: recalled != 0,
                        features_json: features,
                        created_at,
                    }
                },
            )
            .collect())
    }

    /// Delete old training records, keeping the most recent `keep_recent`.
    ///
    /// Called after each retraining cycle to prevent unbounded table growth.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    // TODO(#2416): call cleanup_old_training_data() in the RL retrain loop scheduled in
    // bootstrap/mod.rs once the retrain loop is wired.
    pub async fn cleanup_old_training_data(&self, keep_recent: usize) -> Result<(), MemoryError> {
        let keep = i64::try_from(keep_recent).unwrap_or(i64::MAX);
        zeph_db::query(sql!(
            "DELETE FROM admission_training_data \
             WHERE id NOT IN ( \
                 SELECT id FROM admission_training_data \
                 ORDER BY created_at DESC \
                 LIMIT ? \
             )"
        ))
        .bind(keep)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Save trained RL model weights to `SQLite` for persistence across restarts.
    ///
    /// Uses a fixed `id = 1` row (INSERT OR REPLACE) so the table never grows beyond
    /// one row — avoiding unbounded growth from repeated retrain cycles.
    ///
    /// # Errors
    ///
    /// Returns an error if the database upsert fails.
    pub async fn save_rl_weights(
        &self,
        weights_json: &str,
        sample_count: i64,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT OR REPLACE INTO admission_rl_weights (id, weights_json, sample_count) \
             VALUES (1, ?, ?)"
        ))
        .bind(weights_json)
        .bind(sample_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the latest RL model weights from `SQLite`.
    ///
    /// Returns `None` if no weights have been saved yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_rl_weights(&self) -> Result<Option<(String, i64)>, MemoryError> {
        let row: Option<(String, i64)> = zeph_db::query_as(sql!(
            "SELECT weights_json, sample_count FROM admission_rl_weights \
             ORDER BY id DESC LIMIT 1"
        ))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> (SqliteStore, i64) {
        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
        (store, cid.0)
    }

    #[tokio::test]
    async fn record_and_count_training_data() {
        let (store, cid) = make_store().await;
        let cid = ConversationId(cid);
        store
            .record_admission_training(AdmissionTrainingInput {
                message_id: None,
                conversation_id: cid,
                content: "content",
                role: "user",
                composite_score: 0.5,
                was_admitted: false,
                features_json: "[]",
            })
            .await
            .expect("record rejected");
        store
            .record_admission_training(AdmissionTrainingInput {
                message_id: Some(MessageId(1)),
                conversation_id: cid,
                content: "content2",
                role: "assistant",
                composite_score: 0.8,
                was_admitted: true,
                features_json: "[]",
            })
            .await
            .expect("record admitted");
        let count = store.count_training_records().await.expect("count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn mark_recalled_sets_flag() {
        let (store, cid) = make_store().await;
        let cid = ConversationId(cid);
        store
            .record_admission_training(AdmissionTrainingInput {
                message_id: Some(MessageId(42)),
                conversation_id: cid,
                content: "recalled content",
                role: "user",
                composite_score: 0.7,
                was_admitted: true,
                features_json: "[]",
            })
            .await
            .expect("record");
        store
            .mark_training_recalled(&[MessageId(42)])
            .await
            .expect("mark recalled");
        let batch = store.get_training_batch(10).await.expect("batch");
        assert_eq!(batch.len(), 1);
        assert!(
            batch[0].was_recalled,
            "was_recalled must be true after marking"
        );
    }

    #[tokio::test]
    async fn rejected_message_has_no_message_id() {
        let (store, cid) = make_store().await;
        let cid = ConversationId(cid);
        store
            .record_admission_training(AdmissionTrainingInput {
                message_id: None,
                conversation_id: cid,
                content: "rejected",
                role: "user",
                composite_score: 0.2,
                was_admitted: false,
                features_json: "[]",
            })
            .await
            .expect("record");
        let batch = store.get_training_batch(10).await.expect("batch");
        assert_eq!(batch.len(), 1);
        assert!(!batch[0].was_admitted);
        assert!(batch[0].message_id.is_none());
    }

    #[tokio::test]
    async fn cleanup_trims_old_records() {
        let (store, cid) = make_store().await;
        let cid = ConversationId(cid);
        for i in 0..5_i64 {
            let content = format!("content {i}");
            store
                .record_admission_training(AdmissionTrainingInput {
                    message_id: Some(MessageId(i)),
                    conversation_id: cid,
                    content: &content,
                    role: "user",
                    composite_score: 0.5,
                    was_admitted: true,
                    features_json: "[]",
                })
                .await
                .expect("record");
        }
        // Keep only 2 most recent.
        store.cleanup_old_training_data(2).await.expect("cleanup");
        let count = store.count_training_records().await.expect("count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn save_and_load_rl_weights() {
        let (store, _) = make_store().await;
        store
            .save_rl_weights(r#"{"weights":[0.1,0.2],"bias":0.0}"#, 100)
            .await
            .expect("save");
        let loaded = store.load_rl_weights().await.expect("load");
        assert!(loaded.is_some());
        let (json, count) = loaded.unwrap();
        assert!(json.contains("weights"));
        assert_eq!(count, 100);
    }

    #[tokio::test]
    async fn load_rl_weights_returns_none_when_empty() {
        let (store, _) = make_store().await;
        let loaded = store.load_rl_weights().await.expect("load");
        assert!(loaded.is_none());
    }

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }
}
