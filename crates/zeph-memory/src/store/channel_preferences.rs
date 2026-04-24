// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-channel UX preference storage (#3308).
//!
//! Persists and restores lightweight per-channel settings (currently: active provider name)
//! across agent restarts. The identity key is a composite `(channel_type, channel_id)` pair
//! to support both single-user channels (CLI/TUI) and multi-tenant channels (Telegram, Discord).
//!
//! # Examples
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), zeph_memory::MemoryError> {
//! use zeph_memory::store::DbStore;
//!
//! let store = DbStore::new(":memory:").await?;
//! store.upsert_channel_preference("cli", "", "provider", "fast").await?;
//! let value = store.load_channel_preference("cli", "", "provider").await?;
//! assert_eq!(value.as_deref(), Some("fast"));
//! # Ok(())
//! # }
//! ```

use zeph_db::sql;

use super::SqliteStore;
use crate::error::MemoryError;

impl SqliteStore {
    /// Persist or update a single preference value for a `(channel_type, channel_id)` pair.
    ///
    /// Uses an upsert (INSERT OR REPLACE) so repeated calls with the same key overwrite
    /// the previous value and refresh `updated_at`.
    ///
    /// # Arguments
    ///
    /// - `channel_type` — channel kind: `"cli"`, `"tui"`, `"telegram"`, `"discord"`.
    /// - `channel_id` — user/chat scope within the channel type. Use `""` for CLI/TUI.
    /// - `key` — preference key (e.g. `"provider"`).
    /// - `value` — preference value to store.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the database query fails.
    pub async fn upsert_channel_preference(
        &self,
        channel_type: &str,
        channel_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), MemoryError> {
        // Saturate at i64::MAX (~292 million years from epoch) to avoid clippy::cast_possible_truncation.
        #[allow(clippy::cast_possible_truncation)]
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128),
        )
        .unwrap_or(i64::MAX);

        zeph_db::query(sql!(
            "INSERT INTO channel_preferences \
             (channel_type, channel_id, pref_key, pref_value, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(channel_type, channel_id, pref_key) DO UPDATE SET \
               pref_value = excluded.pref_value, \
               updated_at = excluded.updated_at"
        ))
        .bind(channel_type)
        .bind(channel_id)
        .bind(key)
        .bind(value)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Load a single preference value for a `(channel_type, channel_id)` pair.
    ///
    /// Returns `None` when no value has been stored for the given key.
    ///
    /// # Arguments
    ///
    /// - `channel_type` — channel kind: `"cli"`, `"tui"`, `"telegram"`, `"discord"`.
    /// - `channel_id` — user/chat scope. Use `""` for CLI/TUI.
    /// - `key` — preference key to look up (e.g. `"provider"`).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the database query fails.
    pub async fn load_channel_preference(
        &self,
        channel_type: &str,
        channel_id: &str,
        key: &str,
    ) -> Result<Option<String>, MemoryError> {
        let row: Option<(String,)> = zeph_db::query_as(sql!(
            "SELECT pref_value FROM channel_preferences \
             WHERE channel_type = ? AND channel_id = ? AND pref_key = ?"
        ))
        .bind(channel_type)
        .bind(channel_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(v,)| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn upsert_and_load_roundtrip() {
        let s = store().await;
        s.upsert_channel_preference("cli", "", "provider", "fast")
            .await
            .unwrap();

        let val = s
            .load_channel_preference("cli", "", "provider")
            .await
            .unwrap();
        assert_eq!(val.as_deref(), Some("fast"));
    }

    #[tokio::test]
    async fn upsert_overwrites_existing_value() {
        let s = store().await;
        s.upsert_channel_preference("cli", "", "provider", "fast")
            .await
            .unwrap();
        s.upsert_channel_preference("cli", "", "provider", "quality")
            .await
            .unwrap();

        let val = s
            .load_channel_preference("cli", "", "provider")
            .await
            .unwrap();
        assert_eq!(val.as_deref(), Some("quality"));
    }

    #[tokio::test]
    async fn load_returns_none_when_missing() {
        let s = store().await;
        let val = s
            .load_channel_preference("cli", "", "provider")
            .await
            .unwrap();
        assert!(val.is_none());
    }

    #[tokio::test]
    async fn composite_key_is_unique_per_channel_type_and_id() {
        let s = store().await;
        // Same key, different channel_type → independent values.
        s.upsert_channel_preference("cli", "", "provider", "cli-provider")
            .await
            .unwrap();
        s.upsert_channel_preference("tui", "", "provider", "tui-provider")
            .await
            .unwrap();

        let cli = s
            .load_channel_preference("cli", "", "provider")
            .await
            .unwrap();
        let tui = s
            .load_channel_preference("tui", "", "provider")
            .await
            .unwrap();
        assert_eq!(cli.as_deref(), Some("cli-provider"));
        assert_eq!(tui.as_deref(), Some("tui-provider"));
    }

    #[tokio::test]
    async fn composite_key_is_unique_per_channel_id() {
        let s = store().await;
        // Same channel_type, different channel_id (e.g. Telegram chat IDs).
        s.upsert_channel_preference("telegram", "123", "provider", "fast")
            .await
            .unwrap();
        s.upsert_channel_preference("telegram", "456", "provider", "quality")
            .await
            .unwrap();

        let chat123 = s
            .load_channel_preference("telegram", "123", "provider")
            .await
            .unwrap();
        let chat456 = s
            .load_channel_preference("telegram", "456", "provider")
            .await
            .unwrap();
        assert_eq!(chat123.as_deref(), Some("fast"));
        assert_eq!(chat456.as_deref(), Some("quality"));
    }

    #[tokio::test]
    async fn multiple_keys_per_channel() {
        let s = store().await;
        s.upsert_channel_preference("cli", "", "provider", "fast")
            .await
            .unwrap();
        s.upsert_channel_preference("cli", "", "theme", "dark")
            .await
            .unwrap();

        let provider = s
            .load_channel_preference("cli", "", "provider")
            .await
            .unwrap();
        let theme = s.load_channel_preference("cli", "", "theme").await.unwrap();
        assert_eq!(provider.as_deref(), Some("fast"));
        assert_eq!(theme.as_deref(), Some("dark"));
    }
}
