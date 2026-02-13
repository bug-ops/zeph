use std::str::FromStr;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use zeph_llm::provider::{Message, Role};

#[derive(Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (or create) the `SQLite` database and run migrations.
    ///
    /// Enables foreign key constraints at connection level so that
    /// `ON DELETE CASCADE` and other FK rules are enforced.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn new(path: &str) -> anyhow::Result<Self> {
        let url = if path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite:{path}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .context("failed to open SQLite database")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to run migrations")?;

        Ok(Self { pool })
    }

    /// Expose the underlying pool for shared access by other stores.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Create a new conversation and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn create_conversation(&self) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("INSERT INTO conversations DEFAULT VALUES RETURNING id")
            .fetch_one(&self.pool)
            .await
            .context("failed to create conversation")?;
        Ok(row.0)
    }

    /// Save a message to the given conversation and return the message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message(
        &self,
        conversation_id: i64,
        role: &str,
        content: &str,
    ) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO messages (conversation_id, role, content) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .fetch_one(&self.pool)
        .await
        .context("failed to save message")?;
        Ok(row.0)
    }

    /// Load the most recent messages for a conversation, up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_history(
        &self,
        conversation_id: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<Message>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content FROM (\
                SELECT role, content, id FROM messages \
                WHERE conversation_id = ? \
                ORDER BY id DESC \
                LIMIT ?\
             ) ORDER BY id ASC",
        )
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load history")?;

        let messages = rows
            .into_iter()
            .map(|(role_str, content)| Message {
                role: parse_role(&role_str),
                content,
            })
            .collect();
        Ok(messages)
    }

    /// Return the ID of the most recent conversation, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_conversation_id(&self) -> anyhow::Result<Option<i64>> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM conversations ORDER BY id DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await
                .context("failed to fetch latest conversation")?;
        Ok(row.map(|r| r.0))
    }

    /// Fetch a single message by its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_by_id(&self, message_id: i64) -> anyhow::Result<Option<Message>> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT role, content FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await
                .context("failed to fetch message by id")?;

        Ok(row.map(|(role_str, content)| Message {
            role: parse_role(&role_str),
            content,
        }))
    }

    /// Return message IDs and content for messages without embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unembedded_message_ids(
        &self,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<(i64, i64, String, String)>> {
        let effective_limit = limit.map_or(i64::MAX, |l| i64::try_from(l).unwrap_or(i64::MAX));

        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT m.id, m.conversation_id, m.role, m.content \
             FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL \
             ORDER BY m.id ASC \
             LIMIT ?",
        )
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to fetch unembedded message ids")?;

        Ok(rows)
    }

    /// Count the number of messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages(&self, conversation_id: i64) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await
            .context("failed to count messages")?;
        Ok(row.0)
    }

    /// Count messages in a conversation with id greater than `after_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages_after(
        &self,
        conversation_id: i64,
        after_id: i64,
    ) -> anyhow::Result<i64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND id > ?")
                .bind(conversation_id)
                .bind(after_id)
                .fetch_one(&self.pool)
                .await
                .context("failed to count messages after id")?;
        Ok(row.0)
    }

    /// Load a range of messages after a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_messages_range(
        &self,
        conversation_id: i64,
        after_message_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, String, String)>> {
        let effective_limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, role, content FROM messages \
             WHERE conversation_id = ? AND id > ? \
             ORDER BY id ASC LIMIT ?",
        )
        .bind(conversation_id)
        .bind(after_message_id)
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load messages range")?;

        Ok(rows)
    }

    /// Save a summary and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_summary(
        &self,
        conversation_id: i64,
        content: &str,
        first_message_id: i64,
        last_message_id: i64,
        token_estimate: i64,
    ) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO summaries (conversation_id, content, first_message_id, last_message_id, token_estimate) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(content)
        .bind(first_message_id)
        .bind(last_message_id)
        .bind(token_estimate)
        .fetch_one(&self.pool)
        .await
        .context("failed to save summary")?;
        Ok(row.0)
    }

    /// Load all summaries for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(
        &self,
        conversation_id: i64,
    ) -> anyhow::Result<Vec<(i64, i64, String, i64, i64, i64)>> {
        let rows: Vec<(i64, i64, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT id, conversation_id, content, first_message_id, last_message_id, token_estimate \
             FROM summaries WHERE conversation_id = ? ORDER BY id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .context("failed to load summaries")?;

        Ok(rows)
    }

    /// Get the last message ID covered by the most recent summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_summary_last_message_id(
        &self,
        conversation_id: i64,
    ) -> anyhow::Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT last_message_id FROM summaries \
             WHERE conversation_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch latest summary")?;

        Ok(row.map(|r| r.0))
    }

    /// Record usage of skills (UPSERT: increment count and update timestamp).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn record_skill_usage(&self, skill_names: &[&str]) -> anyhow::Result<()> {
        for name in skill_names {
            sqlx::query(
                "INSERT INTO skill_usage (skill_name, invocation_count, last_used_at) \
                 VALUES (?, 1, datetime('now')) \
                 ON CONFLICT(skill_name) DO UPDATE SET \
                 invocation_count = invocation_count + 1, \
                 last_used_at = datetime('now')",
            )
            .bind(name)
            .execute(&self.pool)
            .await
            .context("failed to record skill usage")?;
        }
        Ok(())
    }

    /// Load all skill usage statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_usage(&self) -> anyhow::Result<Vec<SkillUsageRow>> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT skill_name, invocation_count, last_used_at \
             FROM skill_usage ORDER BY invocation_count DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load skill usage")?;

        Ok(rows
            .into_iter()
            .map(
                |(skill_name, invocation_count, last_used_at)| SkillUsageRow {
                    skill_name,
                    invocation_count,
                    last_used_at,
                },
            )
            .collect())
    }

    // --- Self-learning skill evolution methods ---

    /// Record a skill outcome event.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn record_skill_outcome(
        &self,
        skill_name: &str,
        version_id: Option<i64>,
        conversation_id: Option<i64>,
        outcome: &str,
        error_context: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO skill_outcomes (skill_name, version_id, conversation_id, outcome, error_context) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(skill_name)
        .bind(version_id)
        .bind(conversation_id)
        .bind(outcome)
        .bind(error_context)
        .execute(&self.pool)
        .await
        .context("failed to record skill outcome")?;
        Ok(())
    }

    /// Record outcomes for multiple skills in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if any insert fails (whole batch is rolled back).
    pub async fn record_skill_outcomes_batch(
        &self,
        skill_names: &[String],
        conversation_id: Option<i64>,
        outcome: &str,
        error_context: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await.context("failed to begin tx")?;
        for name in skill_names {
            sqlx::query(
                "INSERT INTO skill_outcomes \
                 (skill_name, version_id, conversation_id, outcome, error_context) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(name)
            .bind(None::<i64>)
            .bind(conversation_id)
            .bind(outcome)
            .bind(error_context)
            .execute(&mut *tx)
            .await
            .context("failed to record skill outcome")?;
        }
        tx.commit().await.context("failed to commit outcomes")?;
        Ok(())
    }

    /// Load metrics for a skill (latest version group).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn skill_metrics(&self, skill_name: &str) -> anyhow::Result<Option<SkillMetricsRow>> {
        let row: Option<(String, Option<i64>, i64, i64, i64)> = sqlx::query_as(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes WHERE skill_name = ? \
             GROUP BY skill_name, version_id \
             ORDER BY version_id DESC LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load skill metrics")?;

        Ok(row.map(
            |(skill_name, version_id, total, successes, failures)| SkillMetricsRow {
                skill_name,
                version_id,
                total,
                successes,
                failures,
            },
        ))
    }

    /// Load all skill outcome stats grouped by skill name.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_outcome_stats(&self) -> anyhow::Result<Vec<SkillMetricsRow>> {
        let rows: Vec<(String, Option<i64>, i64, i64, i64)> = sqlx::query_as(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes \
             GROUP BY skill_name \
             ORDER BY total DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load skill outcome stats")?;

        Ok(rows
            .into_iter()
            .map(
                |(skill_name, version_id, total, successes, failures)| SkillMetricsRow {
                    skill_name,
                    version_id,
                    total,
                    successes,
                    failures,
                },
            )
            .collect())
    }

    /// Save a new skill version and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_skill_version(
        &self,
        skill_name: &str,
        version: i64,
        body: &str,
        description: &str,
        source: &str,
        error_context: Option<&str>,
        predecessor_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO skill_versions \
             (skill_name, version, body, description, source, error_context, predecessor_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(skill_name)
        .bind(version)
        .bind(body)
        .bind(description)
        .bind(source)
        .bind(error_context)
        .bind(predecessor_id)
        .fetch_one(&self.pool)
        .await
        .context("failed to save skill version")?;
        Ok(row.0)
    }

    /// Load the active version for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn active_skill_version(
        &self,
        skill_name: &str,
    ) -> anyhow::Result<Option<SkillVersionRow>> {
        let row: Option<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE skill_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load active skill version")?;

        Ok(row.map(skill_version_from_tuple))
    }

    /// Activate a specific version (deactivates others for the same skill).
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn activate_skill_version(
        &self,
        skill_name: &str,
        version_id: i64,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await.context("failed to begin tx")?;

        sqlx::query(
            "UPDATE skill_versions SET is_active = 0 WHERE skill_name = ? AND is_active = 1",
        )
        .bind(skill_name)
        .execute(&mut *tx)
        .await
        .context("failed to deactivate versions")?;

        sqlx::query("UPDATE skill_versions SET is_active = 1 WHERE id = ?")
            .bind(version_id)
            .execute(&mut *tx)
            .await
            .context("failed to activate version")?;

        tx.commit().await.context("failed to commit activation")?;
        Ok(())
    }

    /// Get the next version number for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn next_skill_version(&self, skill_name: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM skill_versions WHERE skill_name = ?",
        )
        .bind(skill_name)
        .fetch_one(&self.pool)
        .await
        .context("failed to get next version")?;
        Ok(row.0)
    }

    /// Get the latest auto-generated version's `created_at` for cooldown check.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn last_improvement_time(&self, skill_name: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT created_at FROM skill_versions \
             WHERE skill_name = ? AND source = 'auto' \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await
        .context("failed to get last improvement time")?;
        Ok(row.map(|r| r.0))
    }

    /// Ensure a base (v1 manual) version exists for a skill. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB operation fails.
    pub async fn ensure_skill_version_exists(
        &self,
        skill_name: &str,
        body: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM skill_versions WHERE skill_name = ? LIMIT 1")
                .bind(skill_name)
                .fetch_optional(&self.pool)
                .await
                .context("failed to check skill version existence")?;

        if existing.is_none() {
            let id = self
                .save_skill_version(skill_name, 1, body, description, "manual", None, None)
                .await?;
            self.activate_skill_version(skill_name, id).await?;
        }
        Ok(())
    }

    /// Load all versions for a skill, ordered by version number.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_versions(
        &self,
        skill_name: &str,
    ) -> anyhow::Result<Vec<SkillVersionRow>> {
        let rows: Vec<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE skill_name = ? ORDER BY version ASC",
        )
        .bind(skill_name)
        .fetch_all(&self.pool)
        .await
        .context("failed to load skill versions")?;

        Ok(rows.into_iter().map(skill_version_from_tuple).collect())
    }

    /// Count auto-generated versions for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_auto_versions(&self, skill_name: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM skill_versions WHERE skill_name = ? AND source = 'auto'",
        )
        .bind(skill_name)
        .fetch_one(&self.pool)
        .await
        .context("failed to count auto versions")?;
        Ok(row.0)
    }

    /// Delete oldest non-active auto versions exceeding max limit.
    /// Returns the number of pruned versions.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn prune_skill_versions(
        &self,
        skill_name: &str,
        max_versions: u32,
    ) -> anyhow::Result<u32> {
        let result = sqlx::query(
            "DELETE FROM skill_versions WHERE id IN (\
                SELECT id FROM skill_versions \
                WHERE skill_name = ? AND source = 'auto' AND is_active = 0 \
                ORDER BY id ASC \
                LIMIT max(0, (SELECT COUNT(*) FROM skill_versions \
                    WHERE skill_name = ? AND source = 'auto') - ?)\
            )",
        )
        .bind(skill_name)
        .bind(skill_name)
        .bind(max_versions)
        .execute(&self.pool)
        .await
        .context("failed to prune skill versions")?;
        Ok(u32::try_from(result.rows_affected()).unwrap_or(0))
    }

    /// Get the predecessor version for rollback.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn predecessor_version(
        &self,
        version_id: i64,
    ) -> anyhow::Result<Option<SkillVersionRow>> {
        let pred_id: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT predecessor_id FROM skill_versions WHERE id = ?")
                .bind(version_id)
                .fetch_optional(&self.pool)
                .await
                .context("failed to get predecessor id")?;

        let Some((Some(pid),)) = pred_id else {
            return Ok(None);
        };

        let row: Option<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE id = ?",
        )
        .bind(pid)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load predecessor version")?;

        Ok(row.map(skill_version_from_tuple))
    }
}

#[derive(Debug)]
pub struct SkillUsageRow {
    pub skill_name: String,
    pub invocation_count: i64,
    pub last_used_at: String,
}

#[derive(Debug)]
pub struct SkillMetricsRow {
    pub skill_name: String,
    pub version_id: Option<i64>,
    pub total: i64,
    pub successes: i64,
    pub failures: i64,
}

#[derive(Debug)]
pub struct SkillVersionRow {
    pub id: i64,
    pub skill_name: String,
    pub version: i64,
    pub body: String,
    pub description: String,
    pub source: String,
    pub is_active: bool,
    pub success_count: i64,
    pub failure_count: i64,
    pub created_at: String,
}

type SkillVersionTuple = (
    i64,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    String,
);

fn skill_version_from_tuple(t: SkillVersionTuple) -> SkillVersionRow {
    SkillVersionRow {
        id: t.0,
        skill_name: t.1,
        version: t.2,
        body: t.3,
        description: t.4,
        source: t.5,
        is_active: t.6 != 0,
        success_count: t.7,
        failure_count: t.8,
        created_at: t.9,
    }
}

fn parse_role(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    }
}

#[must_use]
pub fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn create_conversation_returns_id() {
        let store = test_store().await;
        let id1 = store.create_conversation().await.unwrap();
        let id2 = store.create_conversation().await.unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[tokio::test]
    async fn save_and_load_messages() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let msg_id2 = store
            .save_message(cid, "assistant", "hi there")
            .await
            .unwrap();

        assert_eq!(msg_id1, 1);
        assert_eq!(msg_id2, 2);

        let history = store.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].content, "hi there");
    }

    #[tokio::test]
    async fn load_history_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        for i in 0..10 {
            store
                .save_message(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let history = store.load_history(cid, 3).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "msg 7");
        assert_eq!(history[1].content, "msg 8");
        assert_eq!(history[2].content, "msg 9");
    }

    #[tokio::test]
    async fn latest_conversation_id_empty() {
        let store = test_store().await;
        assert!(store.latest_conversation_id().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn latest_conversation_id_returns_newest() {
        let store = test_store().await;
        store.create_conversation().await.unwrap();
        let id2 = store.create_conversation().await.unwrap();
        assert_eq!(store.latest_conversation_id().await.unwrap(), Some(id2));
    }

    #[tokio::test]
    async fn messages_isolated_per_conversation() {
        let store = test_store().await;
        let cid1 = store.create_conversation().await.unwrap();
        let cid2 = store.create_conversation().await.unwrap();

        store.save_message(cid1, "user", "conv1").await.unwrap();
        store.save_message(cid2, "user", "conv2").await.unwrap();

        let h1 = store.load_history(cid1, 50).await.unwrap();
        let h2 = store.load_history(cid2, 50).await.unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h1[0].content, "conv1");
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].content, "conv2");
    }

    #[tokio::test]
    async fn pool_accessor_returns_valid_pool() {
        let store = test_store().await;
        let pool = store.pool();
        let row: (i64,) = sqlx::query_as("SELECT 1").fetch_one(pool).await.unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn embeddings_metadata_table_exists() {
        let store = test_store().await;
        let result: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embeddings_metadata'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(result.0, 1);
    }

    #[tokio::test]
    async fn cascade_delete_removes_embeddings_metadata() {
        let store = test_store().await;
        let pool = store.pool();

        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "test").await.unwrap();

        let point_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
             VALUES (?, ?, ?)",
        )
        .bind(msg_id)
        .bind(&point_id)
        .bind(768_i64)
        .execute(pool)
        .await
        .unwrap();

        let before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(msg_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before.0, 1);

        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(msg_id)
            .execute(pool)
            .await
            .unwrap();

        let after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(msg_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(after.0, 0);
    }

    #[tokio::test]
    async fn message_by_id_fetches_existing() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "hello").await.unwrap();

        let msg = store.message_by_id(msg_id).await.unwrap();
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
    }

    #[tokio::test]
    async fn message_by_id_returns_none_for_nonexistent() {
        let store = test_store().await;
        let msg = store.message_by_id(999).await.unwrap();
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn unembedded_message_ids_returns_all_when_none_embedded() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();

        let unembedded = store.unembedded_message_ids(None).await.unwrap();
        assert_eq!(unembedded.len(), 2);
        assert_eq!(unembedded[0].3, "msg1");
        assert_eq!(unembedded[1].3, "msg2");
    }

    #[tokio::test]
    async fn unembedded_message_ids_excludes_embedded() {
        let store = test_store().await;
        let pool = store.pool();
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();

        let point_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
             VALUES (?, ?, ?)",
        )
        .bind(msg_id1)
        .bind(&point_id)
        .bind(768_i64)
        .execute(pool)
        .await
        .unwrap();

        let unembedded = store.unembedded_message_ids(None).await.unwrap();
        assert_eq!(unembedded.len(), 1);
        assert_eq!(unembedded[0].0, msg_id2);
        assert_eq!(unembedded[0].3, "msg2");
    }

    #[tokio::test]
    async fn unembedded_message_ids_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        for i in 0..10 {
            store
                .save_message(cid, "user", &format!("msg{i}"))
                .await
                .unwrap();
        }

        let unembedded = store.unembedded_message_ids(Some(3)).await.unwrap();
        assert_eq!(unembedded.len(), 3);
    }

    #[tokio::test]
    async fn count_messages_returns_correct_count() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        assert_eq!(store.count_messages(cid).await.unwrap(), 0);

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();

        assert_eq!(store.count_messages(cid).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn count_messages_after_filters_correctly() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let _id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
        let _id3 = store.save_message(cid, "user", "msg3").await.unwrap();

        assert_eq!(store.count_messages_after(cid, 0).await.unwrap(), 3);
        assert_eq!(store.count_messages_after(cid, id1).await.unwrap(), 2);
        assert_eq!(store.count_messages_after(cid, _id3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn load_messages_range_basic() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "msg3").await.unwrap();

        let msgs = store.load_messages_range(cid, msg_id1, 10).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].0, msg_id2);
        assert_eq!(msgs[0].2, "msg2");
        assert_eq!(msgs[1].0, msg_id3);
        assert_eq!(msgs[1].2, "msg3");
    }

    #[tokio::test]
    async fn load_messages_range_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();
        store.save_message(cid, "user", "msg3").await.unwrap();

        let msgs = store.load_messages_range(cid, 0, 2).await.unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn save_and_load_summary() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "hi").await.unwrap();

        let summary_id = store
            .save_summary(cid, "User greeted assistant", msg_id1, msg_id2, 5)
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].0, summary_id);
        assert_eq!(summaries[0].2, "User greeted assistant");
        assert_eq!(summaries[0].3, msg_id1);
        assert_eq!(summaries[0].4, msg_id2);
        assert_eq!(summaries[0].5, 5);
    }

    #[tokio::test]
    async fn load_summaries_empty() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn load_summaries_ordered() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        let s1 = store
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        let s2 = store
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].0, s1);
        assert_eq!(summaries[1].0, s2);
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_none() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert!(last.is_none());
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_some() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        store
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        store
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert_eq!(last, Some(msg_id3));
    }

    #[tokio::test]
    async fn cascade_delete_removes_summaries() {
        let store = test_store().await;
        let pool = store.pool();
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();

        store
            .save_summary(cid, "summary", msg_id1, msg_id2, 3)
            .await
            .unwrap();

        let before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before.0, 1);

        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(cid)
            .execute(pool)
            .await
            .unwrap();

        let after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(after.0, 0);
    }

    #[tokio::test]
    async fn record_skill_usage_increments() {
        let store = test_store().await;

        store.record_skill_usage(&["git"]).await.unwrap();
        store.record_skill_usage(&["git"]).await.unwrap();

        let usage = store.load_skill_usage().await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].skill_name, "git");
        assert_eq!(usage[0].invocation_count, 2);
    }

    #[tokio::test]
    async fn load_skill_usage_returns_all() {
        let store = test_store().await;

        store.record_skill_usage(&["git", "docker"]).await.unwrap();
        store.record_skill_usage(&["git"]).await.unwrap();

        let usage = store.load_skill_usage().await.unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].skill_name, "git");
        assert_eq!(usage[0].invocation_count, 2);
        assert_eq!(usage[1].skill_name, "docker");
        assert_eq!(usage[1].invocation_count, 1);
    }

    // --- Self-learning skill evolution tests ---

    #[tokio::test]
    async fn migration_005_creates_tables() {
        let store = test_store().await;
        let pool = store.pool();

        let versions: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_versions'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(versions.0, 1);

        let outcomes: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_outcomes'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(outcomes.0, 1);
    }

    #[tokio::test]
    async fn record_skill_outcome_inserts() {
        let store = test_store().await;

        store
            .record_skill_outcome("git", None, Some(1), "success", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, Some(1), "tool_failure", Some("exit code 1"))
            .await
            .unwrap();

        let metrics = store.skill_metrics("git").await.unwrap().unwrap();
        assert_eq!(metrics.total, 2);
        assert_eq!(metrics.successes, 1);
        assert_eq!(metrics.failures, 1);
    }

    #[tokio::test]
    async fn skill_metrics_none_for_unknown() {
        let store = test_store().await;
        let m = store.skill_metrics("nonexistent").await.unwrap();
        assert!(m.is_none());
    }

    #[tokio::test]
    async fn load_skill_outcome_stats_grouped() {
        let store = test_store().await;

        store
            .record_skill_outcome("git", None, None, "success", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, None, "tool_failure", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("docker", None, None, "success", None)
            .await
            .unwrap();

        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].skill_name, "git");
        assert_eq!(stats[0].total, 2);
        assert_eq!(stats[1].skill_name, "docker");
        assert_eq!(stats[1].total, 1);
    }

    #[tokio::test]
    async fn save_and_load_skill_version() {
        let store = test_store().await;

        let id = store
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        assert!(id > 0);

        store.activate_skill_version("git", id).await.unwrap();

        let active = store.active_skill_version("git").await.unwrap().unwrap();
        assert_eq!(active.version, 1);
        assert_eq!(active.body, "body v1");
        assert!(active.is_active);
    }

    #[tokio::test]
    async fn activate_deactivates_previous() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", v1).await.unwrap();

        let v2 = store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();
        store.activate_skill_version("git", v2).await.unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(!versions[0].is_active);
        assert!(versions[1].is_active);
    }

    #[tokio::test]
    async fn next_skill_version_increments() {
        let store = test_store().await;

        let next = store.next_skill_version("git").await.unwrap();
        assert_eq!(next, 1);

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        let next = store.next_skill_version("git").await.unwrap();
        assert_eq!(next, 2);
    }

    #[tokio::test]
    async fn last_improvement_time_returns_auto_only() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();

        let t = store.last_improvement_time("git").await.unwrap();
        assert!(t.is_none());

        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, None)
            .await
            .unwrap();

        let t = store.last_improvement_time("git").await.unwrap();
        assert!(t.is_some());
    }

    #[tokio::test]
    async fn ensure_skill_version_exists_idempotent() {
        let store = test_store().await;

        store
            .ensure_skill_version_exists("git", "body", "Git helper")
            .await
            .unwrap();
        store
            .ensure_skill_version_exists("git", "body2", "Git helper 2")
            .await
            .unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].body, "body");
    }

    #[tokio::test]
    async fn load_skill_versions_ordered() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[1].version, 2);
    }

    #[tokio::test]
    async fn count_auto_versions_only() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 3, "v3", "desc", "auto", None, None)
            .await
            .unwrap();

        let count = store.count_auto_versions("git").await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn prune_preserves_manual_and_active() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", v1).await.unwrap();

        for i in 2..=5 {
            store
                .save_skill_version("git", i, &format!("v{i}"), "desc", "auto", None, None)
                .await
                .unwrap();
        }

        let pruned = store.prune_skill_versions("git", 2).await.unwrap();
        assert_eq!(pruned, 2);

        let versions = store.load_skill_versions("git").await.unwrap();
        assert!(versions.iter().any(|v| v.source == "manual"));
        let auto_count = versions.iter().filter(|v| v.source == "auto").count();
        assert_eq!(auto_count, 2);
    }

    #[tokio::test]
    async fn predecessor_version_returns_parent() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        let v2 = store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let pred = store.predecessor_version(v2).await.unwrap().unwrap();
        assert_eq!(pred.id, v1);
        assert_eq!(pred.version, 1);
    }

    #[tokio::test]
    async fn predecessor_version_none_for_root() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();

        let pred = store.predecessor_version(v1).await.unwrap();
        assert!(pred.is_none());
    }

    #[tokio::test]
    async fn active_skill_version_none_for_unknown() {
        let store = test_store().await;
        let active = store.active_skill_version("nonexistent").await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn load_skill_outcome_stats_empty() {
        let store = test_store().await;
        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn load_skill_versions_empty() {
        let store = test_store().await;
        let versions = store.load_skill_versions("nonexistent").await.unwrap();
        assert!(versions.is_empty());
    }

    #[tokio::test]
    async fn count_auto_versions_zero_for_unknown() {
        let store = test_store().await;
        let count = store.count_auto_versions("nonexistent").await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn prune_nothing_when_below_limit() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "auto", None, None)
            .await
            .unwrap();

        let pruned = store.prune_skill_versions("git", 5).await.unwrap();
        assert_eq!(pruned, 0);
    }

    #[tokio::test]
    async fn record_skill_outcome_with_error_context() {
        let store = test_store().await;

        store
            .record_skill_outcome(
                "docker",
                None,
                Some(1),
                "tool_failure",
                Some("container not found"),
            )
            .await
            .unwrap();

        let metrics = store.skill_metrics("docker").await.unwrap().unwrap();
        assert_eq!(metrics.total, 1);
        assert_eq!(metrics.failures, 1);
    }

    #[tokio::test]
    async fn save_skill_version_with_error_context() {
        let store = test_store().await;

        let id = store
            .save_skill_version(
                "git",
                1,
                "improved body",
                "Git helper",
                "auto",
                Some("exit code 128"),
                None,
            )
            .await
            .unwrap();
        assert!(id > 0);
    }
}
