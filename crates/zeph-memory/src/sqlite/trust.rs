use super::SqliteStore;
use crate::error::MemoryError;

#[derive(Debug, Clone)]
pub struct SkillTrustRow {
    pub skill_name: String,
    pub trust_level: String,
    pub source_kind: String,
    pub source_url: Option<String>,
    pub source_path: Option<String>,
    pub blake3_hash: String,
    pub updated_at: String,
}

type TrustTuple = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn row_from_tuple(t: TrustTuple) -> SkillTrustRow {
    SkillTrustRow {
        skill_name: t.0,
        trust_level: t.1,
        source_kind: t.2,
        source_url: t.3,
        source_path: t.4,
        blake3_hash: t.5,
        updated_at: t.6,
    }
}

impl SqliteStore {
    /// Upsert trust metadata for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn upsert_skill_trust(
        &self,
        skill_name: &str,
        trust_level: &str,
        source_kind: &str,
        source_url: Option<&str>,
        source_path: Option<&str>,
        blake3_hash: &str,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO skill_trust (skill_name, trust_level, source_kind, source_url, source_path, blake3_hash, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, datetime('now')) \
             ON CONFLICT(skill_name) DO UPDATE SET \
             trust_level = excluded.trust_level, \
             source_kind = excluded.source_kind, \
             source_url = excluded.source_url, \
             source_path = excluded.source_path, \
             blake3_hash = excluded.blake3_hash, \
             updated_at = datetime('now')",
        )
        .bind(skill_name)
        .bind(trust_level)
        .bind(source_kind)
        .bind(source_url)
        .bind(source_path)
        .bind(blake3_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load trust metadata for a single skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_trust(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillTrustRow>, MemoryError> {
        let row: Option<TrustTuple> = sqlx::query_as(
            "SELECT skill_name, trust_level, source_kind, source_url, source_path, blake3_hash, updated_at \
             FROM skill_trust WHERE skill_name = ?",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_from_tuple))
    }

    /// Load all skill trust entries.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_all_skill_trust(&self) -> Result<Vec<SkillTrustRow>, MemoryError> {
        let rows: Vec<TrustTuple> = sqlx::query_as(
            "SELECT skill_name, trust_level, source_kind, source_url, source_path, blake3_hash, updated_at \
             FROM skill_trust ORDER BY skill_name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Update only the trust level for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the skill does not exist or the update fails.
    pub async fn set_skill_trust_level(
        &self,
        skill_name: &str,
        trust_level: &str,
    ) -> Result<bool, MemoryError> {
        let result = sqlx::query(
            "UPDATE skill_trust SET trust_level = ?, updated_at = datetime('now') WHERE skill_name = ?",
        )
        .bind(trust_level)
        .bind(skill_name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete trust entry for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete_skill_trust(&self, skill_name: &str) -> Result<bool, MemoryError> {
        let result = sqlx::query("DELETE FROM skill_trust WHERE skill_name = ?")
            .bind(skill_name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the blake3 hash for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn update_skill_hash(
        &self,
        skill_name: &str,
        blake3_hash: &str,
    ) -> Result<bool, MemoryError> {
        let result = sqlx::query(
            "UPDATE skill_trust SET blake3_hash = ?, updated_at = datetime('now') WHERE skill_name = ?",
        )
        .bind(blake3_hash)
        .bind(skill_name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn upsert_and_load() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "trusted", "local", None, None, "abc123")
            .await
            .unwrap();

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert_eq!(row.skill_name, "git");
        assert_eq!(row.trust_level, "trusted");
        assert_eq!(row.source_kind, "local");
        assert_eq!(row.blake3_hash, "abc123");
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "quarantined", "local", None, None, "hash1")
            .await
            .unwrap();
        store
            .upsert_skill_trust("git", "trusted", "local", None, None, "hash2")
            .await
            .unwrap();

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert_eq!(row.trust_level, "trusted");
        assert_eq!(row.blake3_hash, "hash2");
    }

    #[tokio::test]
    async fn load_nonexistent() {
        let store = test_store().await;
        let row = store.load_skill_trust("nope").await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn load_all() {
        let store = test_store().await;

        store
            .upsert_skill_trust("alpha", "trusted", "local", None, None, "h1")
            .await
            .unwrap();
        store
            .upsert_skill_trust(
                "beta",
                "quarantined",
                "hub",
                Some("https://hub.example.com"),
                None,
                "h2",
            )
            .await
            .unwrap();

        let rows = store.load_all_skill_trust().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].skill_name, "alpha");
        assert_eq!(rows[1].skill_name, "beta");
    }

    #[tokio::test]
    async fn set_trust_level() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "quarantined", "local", None, None, "h1")
            .await
            .unwrap();

        let updated = store.set_skill_trust_level("git", "blocked").await.unwrap();
        assert!(updated);

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert_eq!(row.trust_level, "blocked");
    }

    #[tokio::test]
    async fn set_trust_level_nonexistent() {
        let store = test_store().await;
        let updated = store
            .set_skill_trust_level("nope", "blocked")
            .await
            .unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn delete_trust() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "trusted", "local", None, None, "h1")
            .await
            .unwrap();

        let deleted = store.delete_skill_trust("git").await.unwrap();
        assert!(deleted);

        let row = store.load_skill_trust("git").await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let store = test_store().await;
        let deleted = store.delete_skill_trust("nope").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn update_hash() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "verified", "local", None, None, "old_hash")
            .await
            .unwrap();

        let updated = store.update_skill_hash("git", "new_hash").await.unwrap();
        assert!(updated);

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert_eq!(row.blake3_hash, "new_hash");
    }

    #[tokio::test]
    async fn source_with_url() {
        let store = test_store().await;

        store
            .upsert_skill_trust(
                "remote-skill",
                "quarantined",
                "hub",
                Some("https://hub.example.com/skill"),
                None,
                "h1",
            )
            .await
            .unwrap();

        let row = store
            .load_skill_trust("remote-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_kind, "hub");
        assert_eq!(
            row.source_url.as_deref(),
            Some("https://hub.example.com/skill")
        );
    }

    #[tokio::test]
    async fn source_with_path() {
        let store = test_store().await;

        store
            .upsert_skill_trust(
                "file-skill",
                "quarantined",
                "file",
                None,
                Some("/tmp/skill.tar.gz"),
                "h1",
            )
            .await
            .unwrap();

        let row = store.load_skill_trust("file-skill").await.unwrap().unwrap();
        assert_eq!(row.source_kind, "file");
        assert_eq!(row.source_path.as_deref(), Some("/tmp/skill.tar.gz"));
    }
}
