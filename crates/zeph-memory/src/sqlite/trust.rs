// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use super::SqliteStore;
use crate::error::MemoryError;

/// Discriminant for the skill source stored in the trust table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Local,
    Hub,
    File,
}

impl SourceKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hub => "hub",
            Self::File => "file",
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SourceKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(Self::Local),
            "hub" => Ok(Self::Hub),
            "file" => Ok(Self::File),
            other => Err(format!("unknown source_kind: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillTrustRow {
    pub skill_name: String,
    pub trust_level: String,
    pub source_kind: SourceKind,
    pub source_url: Option<String>,
    pub source_path: Option<String>,
    pub blake3_hash: String,
    pub updated_at: String,
    /// Upstream git commit hash at install time (from `x-git-hash` frontmatter field).
    pub git_hash: Option<String>,
}

type TrustTuple = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
);

fn row_from_tuple(t: TrustTuple) -> SkillTrustRow {
    let source_kind = t.2.parse::<SourceKind>().unwrap_or(SourceKind::Local);
    SkillTrustRow {
        skill_name: t.0,
        trust_level: t.1,
        source_kind,
        source_url: t.3,
        source_path: t.4,
        blake3_hash: t.5,
        updated_at: t.6,
        git_hash: t.7,
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
        source_kind: SourceKind,
        source_url: Option<&str>,
        source_path: Option<&str>,
        blake3_hash: &str,
    ) -> Result<(), MemoryError> {
        self.upsert_skill_trust_with_git_hash(
            skill_name,
            trust_level,
            source_kind,
            source_url,
            source_path,
            blake3_hash,
            None,
        )
        .await
    }

    /// Upsert trust metadata for a skill, including an optional upstream git hash.
    ///
    /// `git_hash` is the upstream commit hash from the `x-git-hash` SKILL.md frontmatter field.
    /// It tracks the upstream commit at install time and is stored separately from `blake3_hash`
    /// (which tracks content integrity).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_skill_trust_with_git_hash(
        &self,
        skill_name: &str,
        trust_level: &str,
        source_kind: SourceKind,
        source_url: Option<&str>,
        source_path: Option<&str>,
        blake3_hash: &str,
        git_hash: Option<&str>,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO skill_trust \
             (skill_name, trust_level, source_kind, source_url, source_path, blake3_hash, git_hash, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now')) \
             ON CONFLICT(skill_name) DO UPDATE SET \
             trust_level = excluded.trust_level, \
             source_kind = excluded.source_kind, \
             source_url = excluded.source_url, \
             source_path = excluded.source_path, \
             blake3_hash = excluded.blake3_hash, \
             git_hash = excluded.git_hash, \
             updated_at = datetime('now')",
        )
        .bind(skill_name)
        .bind(trust_level)
        .bind(source_kind.as_str())
        .bind(source_url)
        .bind(source_path)
        .bind(blake3_hash)
        .bind(git_hash)
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
            "SELECT skill_name, trust_level, source_kind, source_url, source_path, \
             blake3_hash, updated_at, git_hash \
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
            "SELECT skill_name, trust_level, source_kind, source_url, source_path, \
             blake3_hash, updated_at, git_hash \
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
            .upsert_skill_trust("git", "trusted", SourceKind::Local, None, None, "abc123")
            .await
            .unwrap();

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert_eq!(row.skill_name, "git");
        assert_eq!(row.trust_level, "trusted");
        assert_eq!(row.source_kind, SourceKind::Local);
        assert_eq!(row.blake3_hash, "abc123");
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "quarantined", SourceKind::Local, None, None, "hash1")
            .await
            .unwrap();
        store
            .upsert_skill_trust("git", "trusted", SourceKind::Local, None, None, "hash2")
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
            .upsert_skill_trust("alpha", "trusted", SourceKind::Local, None, None, "h1")
            .await
            .unwrap();
        store
            .upsert_skill_trust(
                "beta",
                "quarantined",
                SourceKind::Hub,
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
            .upsert_skill_trust("git", "quarantined", SourceKind::Local, None, None, "h1")
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
            .upsert_skill_trust("git", "trusted", SourceKind::Local, None, None, "h1")
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
            .upsert_skill_trust("git", "verified", SourceKind::Local, None, None, "old_hash")
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
                SourceKind::Hub,
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
        assert_eq!(row.source_kind, SourceKind::Hub);
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
                SourceKind::File,
                None,
                Some("/tmp/skill.tar.gz"),
                "h1",
            )
            .await
            .unwrap();

        let row = store.load_skill_trust("file-skill").await.unwrap().unwrap();
        assert_eq!(row.source_kind, SourceKind::File);
        assert_eq!(row.source_path.as_deref(), Some("/tmp/skill.tar.gz"));
    }

    #[test]
    fn source_kind_display_local() {
        assert_eq!(SourceKind::Local.to_string(), "local");
    }

    #[test]
    fn source_kind_display_hub() {
        assert_eq!(SourceKind::Hub.to_string(), "hub");
    }

    #[test]
    fn source_kind_display_file() {
        assert_eq!(SourceKind::File.to_string(), "file");
    }

    #[test]
    fn source_kind_from_str_local() {
        let kind: SourceKind = "local".parse().unwrap();
        assert_eq!(kind, SourceKind::Local);
    }

    #[test]
    fn source_kind_from_str_hub() {
        let kind: SourceKind = "hub".parse().unwrap();
        assert_eq!(kind, SourceKind::Hub);
    }

    #[test]
    fn source_kind_from_str_file() {
        let kind: SourceKind = "file".parse().unwrap();
        assert_eq!(kind, SourceKind::File);
    }

    #[test]
    fn source_kind_from_str_unknown_returns_error() {
        let result: Result<SourceKind, _> = "s3".parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown source_kind"));
    }

    #[test]
    fn source_kind_serde_json_roundtrip_local() {
        let original = SourceKind::Local;
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#""local""#);
        let back: SourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn source_kind_serde_json_roundtrip_hub() {
        let original = SourceKind::Hub;
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#""hub""#);
        let back: SourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn source_kind_serde_json_roundtrip_file() {
        let original = SourceKind::File;
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#""file""#);
        let back: SourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn source_kind_serde_json_invalid_value_errors() {
        let result: Result<SourceKind, _> = serde_json::from_str(r#""unknown""#);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn trust_row_includes_git_hash() {
        let store = test_store().await;

        store
            .upsert_skill_trust_with_git_hash(
                "versioned-skill",
                "trusted",
                SourceKind::Hub,
                Some("https://hub.example.com/skill"),
                None,
                "blake3abc",
                Some("deadbeef1234"),
            )
            .await
            .unwrap();

        let row = store
            .load_skill_trust("versioned-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.git_hash.as_deref(), Some("deadbeef1234"));
        assert_eq!(row.blake3_hash, "blake3abc");
    }

    #[tokio::test]
    async fn upsert_without_git_hash_leaves_it_null() {
        let store = test_store().await;

        store
            .upsert_skill_trust("git", "trusted", SourceKind::Local, None, None, "hash1")
            .await
            .unwrap();

        let row = store.load_skill_trust("git").await.unwrap().unwrap();
        assert!(row.git_hash.is_none());
    }

    #[tokio::test]
    async fn upsert_each_source_kind_roundtrip() {
        let store = test_store().await;
        let variants = [
            ("skill-local", SourceKind::Local),
            ("skill-hub", SourceKind::Hub),
            ("skill-file", SourceKind::File),
        ];
        for (name, kind) in &variants {
            store
                .upsert_skill_trust(name, "trusted", kind.clone(), None, None, "hash")
                .await
                .unwrap();
            let row = store.load_skill_trust(name).await.unwrap().unwrap();
            assert_eq!(&row.source_kind, kind);
        }
    }
}
