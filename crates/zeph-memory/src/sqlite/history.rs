use super::SqliteStore;
use crate::error::MemoryError;

impl SqliteStore {
    /// Load the most recent input history entries, ordered chronologically.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_input_history(&self, limit: i64) -> Result<Vec<String>, MemoryError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT input FROM input_history ORDER BY id ASC LIMIT ?")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    /// Persist a new input history entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_input_entry(&self, text: &str) -> Result<(), MemoryError> {
        sqlx::query("INSERT INTO input_history (input) VALUES (?)")
            .bind(text)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete all input history entries.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn clear_input_history(&self) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM input_history")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn load_input_history_empty() {
        let store = test_store().await;
        let entries = store.load_input_history(100).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_input_history() {
        let store = test_store().await;
        store.save_input_entry("hello").await.unwrap();
        store.save_input_entry("world").await.unwrap();
        let entries = store.load_input_history(100).await.unwrap();
        assert_eq!(entries, vec!["hello", "world"]);
    }

    #[tokio::test]
    async fn load_input_history_respects_limit() {
        let store = test_store().await;
        for i in 0..10 {
            store.save_input_entry(&format!("entry {i}")).await.unwrap();
        }
        let entries = store.load_input_history(3).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], "entry 0");
    }

    #[tokio::test]
    async fn clear_input_history_removes_all() {
        let store = test_store().await;
        store.save_input_entry("a").await.unwrap();
        store.save_input_entry("b").await.unwrap();
        store.clear_input_history().await.unwrap();
        let entries = store.load_input_history(100).await.unwrap();
        assert!(entries.is_empty());
    }
}
