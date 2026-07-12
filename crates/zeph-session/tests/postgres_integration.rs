// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests for `zeph-session`.
//!
//! These tests require Docker to be running. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-session --features test-utils --test postgres_integration --run-ignored ignored-only
//! ```
//!
//! Regression coverage for issue #5980: `SessionStore::list` bound `LIMIT ?` with `-1` as a
//! `filter.limit == 0` ("unlimited") sentinel. `LIMIT -1` is a `SQLite`-only convenience;
//! `PostgreSQL` rejects a negative `LIMIT` at execution time (`ERROR: LIMIT must not be
//! negative`), so any caller passing `limit = 0` against Postgres got a hard SQL error instead
//! of "all rows". The tests below exercise `SessionStore::list`'s `limit = 0` path against a
//! real Postgres instance.

#[cfg(feature = "test-utils")]
mod pg {
    use std::time::Duration;

    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;
    use zeph_db::DbConfig;
    use zeph_session::store::{SessionFilter, SessionStatus, SessionStore};

    // Generous startup timeout: under concurrent CI load, the default 60s can elapse
    // before Postgres is ready, and testcontainers-rs 0.27.3 leaks the container on a
    // startup-timeout cancel (no Drop guard is ever created).
    async fn start_pg() -> (zeph_db::DbPool, impl Drop) {
        let image = Postgres::default().with_startup_timeout(Duration::from_mins(2));
        let container = image.start().await.expect("docker must be available");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let config = DbConfig { url, pool_size: 5 };
        let pool = config.connect().await.expect("failed to connect to PG");
        (pool, container)
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn list_unlimited_when_zero_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SessionStore::new(pool);

        for i in 0..5u8 {
            store.create(&format!("sess-{i}")).await.unwrap();
        }

        // Regression for #5980: limit=0 must return every row, not error with
        // "LIMIT must not be negative".
        let all = store.list(&SessionFilter::default()).await.unwrap();
        assert_eq!(all.len(), 5);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn list_respects_nonzero_limit_and_status_filter_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SessionStore::new(pool);

        for i in 0..5u8 {
            store.create(&format!("sess-{i}")).await.unwrap();
        }
        store
            .set_status("sess-0", SessionStatus::Archived)
            .await
            .unwrap();

        let limited = store
            .list(&SessionFilter {
                status: None,
                limit: 3,
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 3);

        let active = store
            .list(&SessionFilter {
                status: Some(SessionStatus::Active),
                limit: 0,
            })
            .await
            .unwrap();
        assert_eq!(active.len(), 4);
    }

    // Regression coverage (review follow-up, #5980): `SessionStore::get` and
    // `get_by_conversation_id` received the same `Dialect::select_as_text` `TIMESTAMPTZ` fix as
    // `list()` (all three decode `created_at`/`updated_at` off the same `acp_sessions` columns),
    // but only `list()` was exercised against real Postgres above. Without this test, a future
    // regression isolated to `get`/`get_by_conversation_id` (e.g. a refactor that drops
    // `select_as_text` from one of the three near-identical call sites) would pass the full
    // suite on both backends, since SQLite never had this bug.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn get_and_get_by_conversation_id_decode_timestamps_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SessionStore::new(pool.clone());

        store.create("sess-x").await.unwrap();

        // `conversation_id` carries an FK to `conversations(id)` (migration 001); insert a row
        // directly since creating conversations is zeph-memory's domain, out of scope here
        // (mirrors the SQLite unit test `link_conversation_and_lookup_round_trips` in
        // `src/store.rs`).
        let (cid,): (i64,) = sqlx::query_as(zeph_db::sql!(
            "INSERT INTO conversations DEFAULT VALUES RETURNING id"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        store.link_conversation("sess-x", cid).await.unwrap();

        let by_id = store
            .get("sess-x")
            .await
            .unwrap()
            .expect("session must exist");
        assert_eq!(by_id.session_id, "sess-x");
        assert!(
            !by_id.created_at.is_empty(),
            "created_at must decode as non-empty text"
        );
        assert!(
            !by_id.updated_at.is_empty(),
            "updated_at must decode as non-empty text"
        );

        let by_conv = store
            .get_by_conversation_id(cid)
            .await
            .unwrap()
            .expect("session must be found by conversation_id");
        assert_eq!(by_conv.session_id, "sess-x");
        assert!(!by_conv.created_at.is_empty());
        assert!(!by_conv.updated_at.is_empty());
    }
}
