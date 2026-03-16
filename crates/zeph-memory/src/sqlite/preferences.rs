// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::SqliteStore;
use crate::error::MemoryError;

/// Truncate `s` to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from max_bytes to find a valid char boundary.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[derive(Debug, Clone)]
pub struct LearnedPreferenceRow {
    pub id: i64,
    pub preference_key: String,
    pub preference_value: String,
    pub confidence: f64,
    pub evidence_count: i64,
    pub updated_at: String,
}

type PreferenceTuple = (i64, String, String, f64, i64, String);

fn row_from_tuple(t: PreferenceTuple) -> LearnedPreferenceRow {
    LearnedPreferenceRow {
        id: t.0,
        preference_key: t.1,
        preference_value: t.2,
        confidence: t.3,
        evidence_count: t.4,
        updated_at: t.5,
    }
}

impl SqliteStore {
    /// Insert or update a learned preference.
    ///
    /// When a key already exists, the value and metadata are updated and
    /// `updated_at` is refreshed. `evidence_count` is set to the provided
    /// value (caller is responsible for accumulation logic).
    ///
    /// Keys longer than 128 bytes or values longer than 256 bytes are silently
    /// truncated at a UTF-8 character boundary before storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn upsert_learned_preference(
        &self,
        key: &str,
        value: &str,
        confidence: f64,
        evidence_count: i64,
    ) -> Result<(), MemoryError> {
        const MAX_KEY_BYTES: usize = 128;
        const MAX_VALUE_BYTES: usize = 256;
        let key_trunc = truncate_to_bytes(key, MAX_KEY_BYTES);
        let value_trunc = truncate_to_bytes(value, MAX_VALUE_BYTES);
        if key_trunc.len() < key.len() {
            tracing::warn!(
                original_len = key.len(),
                "learned_preferences: key truncated to 128 bytes"
            );
        }
        if value_trunc.len() < value.len() {
            tracing::warn!(
                original_len = value.len(),
                "learned_preferences: value truncated to 256 bytes"
            );
        }
        sqlx::query(
            "INSERT INTO learned_preferences \
             (preference_key, preference_value, confidence, evidence_count, updated_at) \
             VALUES (?, ?, ?, ?, datetime('now')) \
             ON CONFLICT(preference_key) DO UPDATE SET \
               preference_value = excluded.preference_value, \
               confidence = excluded.confidence, \
               evidence_count = excluded.evidence_count, \
               updated_at = datetime('now')",
        )
        .bind(key_trunc)
        .bind(value_trunc)
        .bind(confidence)
        .bind(evidence_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all learned preferences, ordered by confidence descending.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_learned_preferences(&self) -> Result<Vec<LearnedPreferenceRow>, MemoryError> {
        let rows: Vec<PreferenceTuple> = sqlx::query_as(
            "SELECT id, preference_key, preference_value, confidence, evidence_count, updated_at \
             FROM learned_preferences \
             ORDER BY confidence DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Load corrections with `id > after_id`, ordered by id ascending.
    ///
    /// Used by the learning engine to process only new corrections since the
    /// last analysis run (watermark-based incremental scan).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_corrections_after(
        &self,
        after_id: i64,
        limit: u32,
    ) -> Result<Vec<super::corrections::UserCorrectionRow>, MemoryError> {
        use super::corrections::UserCorrectionRow;

        type Tuple = (
            i64,
            Option<i64>,
            String,
            String,
            Option<String>,
            String,
            String,
        );

        let rows: Vec<Tuple> = sqlx::query_as(
            "SELECT id, session_id, original_output, correction_text, \
             skill_name, correction_kind, created_at \
             FROM user_corrections \
             WHERE id > ? \
             ORDER BY id ASC LIMIT ?",
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|t| UserCorrectionRow {
                id: t.0,
                session_id: t.1,
                original_output: t.2,
                correction_text: t.3,
                skill_name: t.4,
                correction_kind: t.5,
                created_at: t.6,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn upsert_and_load() {
        let s = store().await;
        s.upsert_learned_preference("verbosity", "concise", 0.9, 5)
            .await
            .unwrap();
        let rows = s.load_learned_preferences().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].preference_key, "verbosity");
        assert_eq!(rows[0].preference_value, "concise");
        assert!((rows[0].confidence - 0.9).abs() < 1e-9);
        assert_eq!(rows[0].evidence_count, 5);
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let s = store().await;
        s.upsert_learned_preference("verbosity", "concise", 0.8, 3)
            .await
            .unwrap();
        s.upsert_learned_preference("verbosity", "verbose", 0.95, 8)
            .await
            .unwrap();
        let rows = s.load_learned_preferences().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].preference_value, "verbose");
        assert!((rows[0].confidence - 0.95).abs() < 1e-9);
        assert_eq!(rows[0].evidence_count, 8);
    }

    #[tokio::test]
    async fn load_ordered_by_confidence() {
        let s = store().await;
        s.upsert_learned_preference("format_preference", "bullet points", 0.75, 3)
            .await
            .unwrap();
        s.upsert_learned_preference("verbosity", "concise", 0.9, 5)
            .await
            .unwrap();
        let rows = s.load_learned_preferences().await.unwrap();
        assert_eq!(rows[0].preference_key, "verbosity");
        assert_eq!(rows[1].preference_key, "format_preference");
    }

    #[tokio::test]
    async fn load_corrections_after_watermark() {
        let s = store().await;
        // Insert two corrections
        s.store_user_correction(None, "output", "be brief", None, "explicit_rejection")
            .await
            .unwrap();
        let id2 = s
            .store_user_correction(None, "output2", "use bullets", None, "alternative_request")
            .await
            .unwrap();
        // Watermark at id2-1 => only id2 returned
        let rows = s.load_corrections_after(id2 - 1, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].correction_text, "use bullets");
    }
}
