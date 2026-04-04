// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed store for the compression quality predictor (#2460).
//!
//! Provides persistence for training samples and model weights following the
//! same pattern as `admission_training.rs`.

use zeph_db::sql;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::ConversationId;

/// A single training record for the compression quality predictor.
#[derive(Debug, Clone)]
pub struct CompressionTrainingRecord {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub compression_ratio: f32,
    pub message_count: i64,
    pub avg_message_length: f32,
    pub tool_output_fraction: f32,
    pub probe_score: f32,
    pub created_at: String,
}

impl SqliteStore {
    /// Record a compression probe result for predictor training.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn record_compression_training(
        &self,
        conversation_id: ConversationId,
        compression_ratio: f32,
        message_count: i64,
        avg_message_length: f32,
        tool_output_fraction: f32,
        probe_score: f32,
    ) -> Result<i64, MemoryError> {
        let id = zeph_db::query_scalar(sql!(
            "INSERT INTO compression_predictor_training \
             (conversation_id, compression_ratio, message_count, \
              avg_message_length, tool_output_fraction, probe_score) \
             VALUES (?, ?, ?, ?, ?, ?) \
             RETURNING id"
        ))
        .bind(conversation_id.0)
        .bind(f64::from(compression_ratio))
        .bind(message_count)
        .bind(f64::from(avg_message_length))
        .bind(f64::from(tool_output_fraction))
        .bind(f64::from(probe_score))
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Count total compression training records.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_compression_training_records(&self) -> Result<i64, MemoryError> {
        let count =
            zeph_db::query_scalar(sql!("SELECT COUNT(*) FROM compression_predictor_training"))
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// Get the most recent `limit` training records for model training (sliding window).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn get_compression_training_batch(
        &self,
        limit: usize,
    ) -> Result<Vec<CompressionTrainingRecord>, MemoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = zeph_db::query_as::<_, (i64, i64, f64, i64, f64, f64, f64, String)>(sql!(
            "SELECT id, conversation_id, compression_ratio, message_count, \
                    avg_message_length, tool_output_fraction, probe_score, created_at \
             FROM compression_predictor_training \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, cid, ratio, msg_count, avg_len, tool_frac, score, created_at)| {
                    CompressionTrainingRecord {
                        id,
                        conversation_id: ConversationId(cid),
                        #[expect(clippy::cast_possible_truncation)]
                        compression_ratio: ratio as f32,
                        message_count: msg_count,
                        #[expect(clippy::cast_possible_truncation)]
                        avg_message_length: avg_len as f32,
                        #[expect(clippy::cast_possible_truncation)]
                        tool_output_fraction: tool_frac as f32,
                        #[expect(clippy::cast_possible_truncation)]
                        probe_score: score as f32,
                        created_at,
                    }
                },
            )
            .collect())
    }

    /// Trim compression training records, keeping the most recent `keep_recent`.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn trim_compression_training_data(
        &self,
        keep_recent: usize,
    ) -> Result<(), MemoryError> {
        let keep = i64::try_from(keep_recent).unwrap_or(i64::MAX);
        zeph_db::query(sql!(
            "DELETE FROM compression_predictor_training \
             WHERE id NOT IN ( \
                 SELECT id FROM compression_predictor_training \
                 ORDER BY created_at DESC \
                 LIMIT ? \
             )"
        ))
        .bind(keep)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Save compression predictor weights (singleton row, id = 1).
    ///
    /// # Errors
    ///
    /// Returns an error if the upsert fails.
    pub async fn save_compression_predictor_weights(
        &self,
        weights_json: &str,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT OR REPLACE INTO compression_predictor_weights (id, weights_json, updated_at) \
             VALUES (1, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))"
        ))
        .bind(weights_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load compression predictor weights.
    ///
    /// Returns `None` if no weights have been saved yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_compression_predictor_weights(&self) -> Result<Option<String>, MemoryError> {
        let row: Option<(String,)> = zeph_db::query_as(sql!(
            "SELECT weights_json FROM compression_predictor_weights WHERE id = 1"
        ))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(json,)| json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn record_and_count_training_data() {
        let (store, cid) = make_store().await;
        store
            .record_compression_training(cid, 0.5, 20, 150.0, 0.3, 0.75)
            .await
            .expect("record");
        let count = store
            .count_compression_training_records()
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn batch_returns_records() {
        let (store, cid) = make_store().await;
        store
            .record_compression_training(cid, 0.5, 20, 150.0, 0.3, 0.75)
            .await
            .expect("record");
        let batch = store
            .get_compression_training_batch(10)
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        assert!((batch[0].compression_ratio - 0.5).abs() < 1e-4);
        assert!((batch[0].probe_score - 0.75).abs() < 1e-4);
    }

    #[tokio::test]
    async fn trim_keeps_most_recent() {
        let (store, cid) = make_store().await;
        for _ in 0..5 {
            store
                .record_compression_training(cid, 0.5, 20, 150.0, 0.3, 0.75)
                .await
                .expect("record");
        }
        store.trim_compression_training_data(2).await.expect("trim");
        let count = store
            .count_compression_training_records()
            .await
            .expect("count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn save_and_load_weights() {
        let (store, _) = make_store().await;
        store
            .save_compression_predictor_weights(r#"{"weights":[0.1,0.2,0.3,0.4],"bias":0.0}"#)
            .await
            .expect("save");
        let loaded = store
            .load_compression_predictor_weights()
            .await
            .expect("load");
        assert!(loaded.is_some());
        assert!(loaded.unwrap().contains("weights"));
    }

    #[tokio::test]
    async fn load_weights_returns_none_when_empty() {
        let (store, _) = make_store().await;
        let loaded = store
            .load_compression_predictor_weights()
            .await
            .expect("load");
        assert!(loaded.is_none());
    }
}
