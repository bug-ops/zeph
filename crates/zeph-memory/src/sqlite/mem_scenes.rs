// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SQLite` operations for `MemScene` consolidation (#2332).

use crate::scenes::MemScene;
use crate::types::{MemSceneId, MessageId};

use crate::error::MemoryError;

use super::SqliteStore;

impl SqliteStore {
    /// Fetch semantic-tier messages not yet assigned to any scene.
    ///
    /// Returns `(message_id, content)` pairs ordered by `id` ASC.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn find_unscened_semantic_messages(
        &self,
        limit: usize,
    ) -> Result<Vec<(MessageId, String)>, MemoryError> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(i64, String)> = sqlx::query_as(
            r"
            SELECT m.id, m.content
            FROM messages m
            WHERE m.tier = 'semantic'
              AND m.deleted_at IS NULL
              AND m.id NOT IN (SELECT message_id FROM mem_scene_members)
            ORDER BY m.id ASC
            LIMIT ?
            ",
        )
        .bind(limit_i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, content)| (MessageId(id), content))
            .collect())
    }

    /// Insert a new `MemScene` and link member messages to it.
    ///
    /// Returns the new scene's ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` insert fails.
    pub async fn insert_mem_scene(
        &self,
        label: &str,
        profile: &str,
        member_ids: &[MessageId],
    ) -> Result<MemSceneId, MemoryError> {
        let member_count = i64::try_from(member_ids.len()).unwrap_or(0);
        let mut tx = self.pool.begin().await?;

        let row: (i64,) = sqlx::query_as(
            "INSERT INTO mem_scenes (label, profile, member_count) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(label)
        .bind(profile)
        .bind(member_count)
        .fetch_one(&mut *tx)
        .await?;
        let scene_id = row.0;

        for &msg_id in member_ids {
            sqlx::query(
                "INSERT OR IGNORE INTO mem_scene_members (scene_id, message_id) VALUES (?, ?)",
            )
            .bind(scene_id)
            .bind(msg_id.0)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(MemSceneId(scene_id))
    }

    /// List all `MemScenes` ordered by creation time descending.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn list_mem_scenes(&self) -> Result<Vec<MemScene>, MemoryError> {
        let rows: Vec<(i64, String, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT id, label, profile, member_count, created_at, updated_at \
             FROM mem_scenes ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, label, profile, member_count, created_at, updated_at)| MemScene {
                    id: MemSceneId(id),
                    label,
                    profile,
                    member_count: u32::try_from(member_count).unwrap_or(0),
                    created_at,
                    updated_at,
                },
            )
            .collect())
    }

    /// Fetch member message IDs for a given scene.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn scene_member_ids(
        &self,
        scene_id: MemSceneId,
    ) -> Result<Vec<MessageId>, MemoryError> {
        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT message_id FROM mem_scene_members WHERE scene_id = ?")
                .bind(scene_id.0)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.into_iter().map(|(id,)| MessageId(id)).collect())
    }

    /// Delete all `MemScenes` and their memberships (reset for re-clustering).
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` delete fails.
    pub async fn reset_mem_scenes(&self) -> Result<u64, MemoryError> {
        let result = sqlx::query("DELETE FROM mem_scenes")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    /// Create N real messages in the DB and return their IDs.
    async fn seed_messages(store: &SqliteStore, n: usize) -> Vec<MessageId> {
        let cid = store.create_conversation().await.unwrap();
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let id = store
                .save_message(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
            ids.push(id);
        }
        ids
    }

    // Test: insert_mem_scene creates a scene and links member messages.
    #[tokio::test]
    async fn insert_and_list_scene() {
        let store = make_store().await;
        let ids = seed_messages(&store, 2).await;

        let scene_id = store
            .insert_mem_scene("Rust Auth", "JWT tokens used for RS256.", &ids)
            .await
            .unwrap();
        assert!(scene_id.0 > 0, "scene id must be positive");

        let scenes = store.list_mem_scenes().await.unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].label, "Rust Auth");
        assert_eq!(scenes[0].member_count, 2);
    }

    // Test: scene_member_ids returns linked message IDs (scene member expansion on demand).
    #[tokio::test]
    async fn scene_member_ids_expansion() {
        let store = make_store().await;
        let ids = seed_messages(&store, 3).await;

        let scene_id = store
            .insert_mem_scene("Topic A", "Profile text.", &ids)
            .await
            .unwrap();

        let members = store.scene_member_ids(scene_id).await.unwrap();
        assert_eq!(members.len(), 3);
        for id in &ids {
            assert!(members.contains(id), "member {id} must be in expansion");
        }
    }

    // Test: find_unscened_semantic_messages excludes already-assigned messages.
    #[tokio::test]
    async fn find_unscened_excludes_assigned_members() {
        let store = make_store().await;
        let ids = seed_messages(&store, 3).await;

        // Promote all to semantic tier.
        for id in &ids {
            sqlx::query("UPDATE messages SET tier = 'semantic' WHERE id = ?")
                .bind(id.0)
                .execute(store.pool())
                .await
                .unwrap();
        }

        // All three should appear as unscened.
        let unscened = store.find_unscened_semantic_messages(100).await.unwrap();
        assert_eq!(unscened.len(), 3);

        // Assign first two to a scene.
        store
            .insert_mem_scene("Partial Scene", "Some profile", &ids[..2])
            .await
            .unwrap();

        // Now only the third should be unscened.
        let unscened_after = store.find_unscened_semantic_messages(100).await.unwrap();
        assert_eq!(unscened_after.len(), 1);
        assert_eq!(unscened_after[0].0, ids[2]);
    }

    // Test: reset_mem_scenes clears all scenes and allows re-clustering.
    #[tokio::test]
    async fn reset_scenes_clears_all() {
        let store = make_store().await;
        let ids1 = seed_messages(&store, 1).await;
        let ids2 = seed_messages(&store, 1).await;

        store
            .insert_mem_scene("Scene 1", "Profile 1", &ids1)
            .await
            .unwrap();
        store
            .insert_mem_scene("Scene 2", "Profile 2", &ids2)
            .await
            .unwrap();

        let deleted = store.reset_mem_scenes().await.unwrap();
        assert_eq!(deleted, 2);

        let scenes = store.list_mem_scenes().await.unwrap();
        assert!(scenes.is_empty());
    }

    // Test: list_mem_scenes returns newest first (by id DESC as proxy for created_at DESC).
    // unixepoch() has 1-second resolution — use explicit created_at override via INSERT to
    // guarantee ordering even when both inserts happen within the same second.
    #[tokio::test]
    async fn list_scenes_ordered_newest_first() {
        let store = make_store().await;
        let ids1 = seed_messages(&store, 1).await;
        let ids2 = seed_messages(&store, 1).await;

        // Insert directly with distinct created_at values to avoid single-second collision.
        sqlx::query(
            "INSERT INTO mem_scenes (label, profile, member_count, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("First")
        .bind("Profile first")
        .bind(1i64)
        .bind(1_000_000i64)
        .bind(1_000_000i64)
        .execute(store.pool())
        .await
        .unwrap();
        let scene1_id: (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
            .fetch_one(store.pool())
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO mem_scenes (label, profile, member_count, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("Second")
        .bind("Profile second")
        .bind(1i64)
        .bind(2_000_000i64)
        .bind(2_000_000i64)
        .execute(store.pool())
        .await
        .unwrap();
        let scene2_id: (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
            .fetch_one(store.pool())
            .await
            .unwrap();

        // Link messages to satisfy FK.
        sqlx::query("INSERT INTO mem_scene_members (scene_id, message_id) VALUES (?, ?)")
            .bind(scene1_id.0)
            .bind(ids1[0].0)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO mem_scene_members (scene_id, message_id) VALUES (?, ?)")
            .bind(scene2_id.0)
            .bind(ids2[0].0)
            .execute(store.pool())
            .await
            .unwrap();

        let scenes = store.list_mem_scenes().await.unwrap();
        // "Second" has created_at=2_000_000 > "First" created_at=1_000_000 → Second comes first.
        assert_eq!(scenes.len(), 2);
        assert_eq!(scenes[0].label, "Second");
        assert_eq!(scenes[1].label, "First");
    }
}
