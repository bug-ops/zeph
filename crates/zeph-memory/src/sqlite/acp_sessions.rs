use crate::error::MemoryError;
use crate::sqlite::SqliteStore;

pub struct AcpSessionEvent {
    pub event_type: String,
    pub payload: String,
}

impl SqliteStore {
    /// Create a new ACP session record.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn create_acp_session(&self, session_id: &str) -> Result<(), MemoryError> {
        sqlx::query("INSERT OR IGNORE INTO acp_sessions (id) VALUES (?)")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a single ACP session event.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn save_acp_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO acp_session_events (session_id, event_type, payload) VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(event_type)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all events for an ACP session in insertion order.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_acp_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<AcpSessionEvent>, MemoryError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT event_type, payload FROM acp_session_events WHERE session_id = ? ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(event_type, payload)| AcpSessionEvent {
                event_type,
                payload,
            })
            .collect())
    }

    /// Delete an ACP session and its events (cascade).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete_acp_session(&self, session_id: &str) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM acp_sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Check whether an ACP session record exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn acp_session_exists(&self, session_id: &str) -> Result<bool, MemoryError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acp_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new")
    }

    #[tokio::test]
    async fn create_and_exists() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        assert!(store.acp_session_exists("sess-1").await.unwrap());
        assert!(!store.acp_session_exists("sess-2").await.unwrap());
    }

    #[tokio::test]
    async fn save_and_load_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store
            .save_acp_event("sess-1", "agent_message", "world")
            .await
            .unwrap();

        let events = store.load_acp_events("sess-1").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "user_message");
        assert_eq!(events[0].payload, "hello");
        assert_eq!(events[1].event_type, "agent_message");
        assert_eq!(events[1].payload, "world");
    }

    #[tokio::test]
    async fn delete_cascades_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store.delete_acp_session("sess-1").await.unwrap();

        assert!(!store.acp_session_exists("sess-1").await.unwrap());
        let events = store.load_acp_events("sess-1").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn load_events_empty_for_unknown() {
        let store = make_store().await;
        let events = store.load_acp_events("no-such").await.unwrap();
        assert!(events.is_empty());
    }
}
