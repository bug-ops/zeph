// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests.
//!
//! These tests require Docker to be running and are skipped in CI unless the
//! `test-postgres` CI job is active. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-db --features test-utils --ignored
//! ```

#[cfg(feature = "test-utils")]
mod pg {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;
    use zeph_db::DbConfig;

    async fn start_pg() -> (zeph_db::DbPool, impl Drop) {
        let image = Postgres::default();
        let container = image.start().await.expect("docker must be available");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let config = DbConfig {
            url,
            max_connections: 5,
            pool_size: 5,
        };
        let pool = config.connect().await.expect("failed to connect to PG");
        (pool, container)
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn migrations_apply_cleanly() {
        let (_pool, _container) = start_pg().await;
        // connect() runs migrations internally; reaching here means all 52 passed.
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn migrations_are_idempotent() {
        let (pool, _container) = start_pg().await;
        // Re-run migrations on an already-migrated database — must succeed without error.
        zeph_db::run_migrations(&pool)
            .await
            .expect("re-running migrations must be idempotent");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn basic_insert_select_delete() {
        let (pool, _container) = start_pg().await;
        // Insert a conversation.
        let (id,): (i64,) = sqlx::query_as(zeph_db::sql!(
            "INSERT INTO conversations DEFAULT VALUES RETURNING id"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(id > 0);

        // Insert a message.
        let (mid,): (i64,) = sqlx::query_as(zeph_db::sql!(
            "INSERT INTO messages (conversation_id, role, content) VALUES (?, 'user', 'hello') RETURNING id"
        ))
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(mid > 0);

        // Delete the conversation (cascade).
        let result = sqlx::query(zeph_db::sql!("DELETE FROM conversations WHERE id = ?"))
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(result.rows_affected(), 1);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn fts_trigger_and_search() {
        let (pool, _container) = start_pg().await;

        // Insert a conversation and a message; the tsvector trigger must fire.
        let (cid,): (i64,) = sqlx::query_as(zeph_db::sql!(
            "INSERT INTO conversations DEFAULT VALUES RETURNING id"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query(zeph_db::sql!(
            "INSERT INTO messages (conversation_id, role, content) \
             VALUES (?, 'user', 'the quick brown fox jumps')"
        ))
        .bind(cid)
        .execute(&pool)
        .await
        .unwrap();

        // The trigger should have populated tsv; search for a word from the content.
        let rows: Vec<(i64,)> = sqlx::query_as(zeph_db::sql!(
            "SELECT m.id FROM messages m \
             WHERE m.tsv @@ plainto_tsquery('english', ?) \
             AND m.conversation_id = ?"
        ))
        .bind("fox")
        .bind(cid)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(!rows.is_empty(), "FTS trigger must populate tsv on INSERT");
    }
}
