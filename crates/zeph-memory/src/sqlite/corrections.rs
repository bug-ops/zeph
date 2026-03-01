// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::SqliteStore;
use crate::error::MemoryError;

#[derive(Debug, Clone)]
pub struct UserCorrectionRow {
    pub id: i64,
    pub session_id: Option<i64>,
    pub original_output: String,
    pub correction_text: String,
    pub skill_name: Option<String>,
    pub correction_kind: String,
    pub created_at: String,
}

type CorrectionTuple = (
    i64,
    Option<i64>,
    String,
    String,
    Option<String>,
    String,
    String,
);

fn row_from_tuple(t: CorrectionTuple) -> UserCorrectionRow {
    UserCorrectionRow {
        id: t.0,
        session_id: t.1,
        original_output: t.2,
        correction_text: t.3,
        skill_name: t.4,
        correction_kind: t.5,
        created_at: t.6,
    }
}

impl SqliteStore {
    /// Store a user correction and return the new row ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn store_user_correction(
        &self,
        session_id: Option<i64>,
        original_output: &str,
        correction_text: &str,
        skill_name: Option<&str>,
        correction_kind: &str,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO user_corrections \
             (session_id, original_output, correction_text, skill_name, correction_kind) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(session_id)
        .bind(original_output)
        .bind(correction_text)
        .bind(skill_name)
        .bind(correction_kind)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Load corrections for a specific skill, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_corrections_for_skill(
        &self,
        skill_name: &str,
        limit: u32,
    ) -> Result<Vec<UserCorrectionRow>, MemoryError> {
        let rows: Vec<CorrectionTuple> = sqlx::query_as(
            "SELECT id, session_id, original_output, correction_text, \
             skill_name, correction_kind, created_at \
             FROM user_corrections WHERE skill_name = ? \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(skill_name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Load the most recent corrections across all skills.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_recent_corrections(
        &self,
        limit: u32,
    ) -> Result<Vec<UserCorrectionRow>, MemoryError> {
        let rows: Vec<CorrectionTuple> = sqlx::query_as(
            "SELECT id, session_id, original_output, correction_text, \
             skill_name, correction_kind, created_at \
             FROM user_corrections ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Load a correction by ID (used by vector retrieval path).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_corrections_for_id(
        &self,
        id: i64,
    ) -> Result<Vec<UserCorrectionRow>, MemoryError> {
        let rows: Vec<CorrectionTuple> = sqlx::query_as(
            "SELECT id, session_id, original_output, correction_text, \
             skill_name, correction_kind, created_at \
             FROM user_corrections WHERE id = ?",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn store_and_load_correction() {
        let store = test_store().await;

        let id = store
            .store_user_correction(
                Some(1),
                "original assistant output",
                "that was wrong, try again",
                Some("git"),
                "explicit_rejection",
            )
            .await
            .unwrap();
        assert!(id > 0);

        let rows = store.load_corrections_for_skill("git", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].correction_kind, "explicit_rejection");
        assert_eq!(rows[0].skill_name.as_deref(), Some("git"));
    }

    #[tokio::test]
    async fn load_recent_corrections_ordered() {
        let store = test_store().await;

        store
            .store_user_correction(None, "out1", "fix1", None, "explicit_rejection")
            .await
            .unwrap();
        store
            .store_user_correction(None, "out2", "fix2", None, "alternative_request")
            .await
            .unwrap();

        let rows = store.load_recent_corrections(10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].correction_text, "fix2");
        assert_eq!(rows[1].correction_text, "fix1");
    }

    #[tokio::test]
    async fn load_corrections_for_id_returns_single() {
        let store = test_store().await;

        let id = store
            .store_user_correction(None, "out", "fix", Some("docker"), "repetition")
            .await
            .unwrap();

        let rows = store.load_corrections_for_id(id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
    }

    #[tokio::test]
    async fn load_corrections_for_id_unknown_returns_empty() {
        let store = test_store().await;
        let rows = store.load_corrections_for_id(9999).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn load_corrections_for_skill_unknown_returns_empty() {
        let store = test_store().await;
        let rows = store
            .load_corrections_for_skill("nonexistent", 10)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn load_recent_corrections_empty_table() {
        let store = test_store().await;
        let rows = store.load_recent_corrections(10).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn store_correction_without_skill_name() {
        let store = test_store().await;

        let id = store
            .store_user_correction(
                None,
                "original output",
                "correction text",
                None,
                "repetition",
            )
            .await
            .unwrap();
        assert!(id > 0);

        let rows = store.load_recent_corrections(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].skill_name.is_none());
        assert_eq!(rows[0].correction_kind, "repetition");
    }

    #[tokio::test]
    async fn load_corrections_for_skill_respects_limit() {
        let store = test_store().await;

        for i in 0..5 {
            store
                .store_user_correction(
                    None,
                    &format!("out{i}"),
                    &format!("fix{i}"),
                    Some("git"),
                    "explicit_rejection",
                )
                .await
                .unwrap();
        }

        let rows = store.load_corrections_for_skill("git", 3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }
}
