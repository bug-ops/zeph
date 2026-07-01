// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests for `zeph-index`.
//!
//! These tests require Docker to be running. They run in CI as part of the
//! `build-tests`/`integration` jobs in `.github/workflows/ci.yml`. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-index --features test-utils --test postgres_integration --run-ignored ignored-only
//! ```

#[cfg(feature = "test-utils")]
mod pg {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;
    use zeph_db::DbConfig;
    use zeph_index::store::CodeStore;
    use zeph_memory::QdrantOps;

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

    fn code_store(pool: zeph_db::DbPool) -> CodeStore {
        // No Qdrant call is made by `existing_hashes`, so an unreachable URL is fine —
        // the client connects lazily and is never used in these tests.
        let ops = QdrantOps::new("http://127.0.0.1:1", None).expect("build QdrantOps");
        CodeStore::with_ops(ops, pool)
    }

    /// Regression test for issue #5364: `existing_hashes()` built its `IN (...)` list with
    /// hand-rolled `?` placeholders that were never converted to Postgres `$N` syntax, so
    /// under the `postgres` feature the bind parameters never matched any placeholder and
    /// the query would either error or silently return zero rows.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn existing_hashes_in_list_works_on_postgres() {
        let (pool, _container) = start_pg().await;
        let store = code_store(pool.clone());

        // Insert two known chunk_metadata rows directly.
        for (hash, path) in [("hash1", "a.rs"), ("hash2", "b.rs")] {
            sqlx::query(zeph_db::sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, 1, 2, 'rust', 'function')"
            ))
            .bind(format!("point-{hash}"))
            .bind(path)
            .bind(hash)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Query a chunk size that forces the IN-list path: 2 existing hashes + 1 missing.
        let found = store
            .existing_hashes(&["hash1", "hash2", "hash3"])
            .await
            .unwrap();

        assert_eq!(
            found.len(),
            2,
            "only hash1 and hash2 were actually inserted"
        );
        assert!(found.contains("hash1"));
        assert!(found.contains("hash2"));
        assert!(!found.contains("hash3"));
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn existing_hashes_empty_input_returns_empty_without_querying() {
        let (pool, _container) = start_pg().await;
        let store = code_store(pool);

        let found = store.existing_hashes(&[]).await.unwrap();
        assert!(found.is_empty());
    }
}
