// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests for `zeph-mcp`.
//!
//! These tests require Docker to be running. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-mcp --no-default-features \
//!     --features test-utils --test postgres_integration --run-ignored ignored-only
//! ```
//!
//! Regression coverage for issue #5803: `TrustScoreStore::apply_delta` and
//! `load_and_apply_delta` build `INSERT ... ON CONFLICT(server_id) DO UPDATE SET` statements
//! with unqualified self-references (e.g. `success_count = success_count + excluded.success_count`).
//! Postgres's `ON CONFLICT DO UPDATE` always exposes an implicit `excluded` pseudo-table
//! alongside the target table, so an unqualified self-reference is rejected as ambiguous
//! (`column reference "success_count" is ambiguous`). `SQLite` accepts the same statement,
//! so the existing `store_*` unit tests (`trust_score.rs`, in-memory `SqliteStore`) never
//! caught it. These tests exercise both methods' upsert branch against a real Postgres
//! instance.
//!
//! Also covers a co-located defect found while adding this coverage: `load()`/`load_all()`
//! decoded `success_count`/`failure_count` as `i64`, but both columns are `INTEGER` (INT4)
//! in the Postgres schema (migration `052_mcp_trust_scores.sql`), which `sqlx-postgres`
//! rejects as `ColumnDecode`. `load_all_decodes_int4_counts_on_postgres` closes the gap for
//! `load_all()` specifically (the other tests already exercise `load()` via `apply_delta`
//! and `load_and_apply_delta`'s round trips).

#![cfg(feature = "test-utils")]

use std::time::Duration;

use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;
use zeph_db::DbConfig;
use zeph_mcp::{ServerTrustScore, TrustScoreStore};

// Generous startup timeout, matching the zeph-memory pattern: under concurrent CI load the
// default 60s can elapse before Postgres is ready.
async fn start_pg() -> (zeph_db::DbPool, impl Drop) {
    let image = Postgres::default().with_startup_timeout(Duration::from_mins(2));
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
async fn apply_delta_upsert_increments_counters_on_postgres() {
    let (pool, _container) = start_pg().await;
    let store = TrustScoreStore::new(pool);
    store.init().await.unwrap();

    // First call is a plain INSERT; the second hits the ON CONFLICT DO UPDATE branch —
    // the exact path that previously errored on Postgres with an ambiguous column reference.
    store.apply_delta("srv1", 0.02, 1, 0).await.unwrap();
    store.apply_delta("srv1", 0.02, 1, 0).await.unwrap();

    let loaded = store.load("srv1").await.unwrap().unwrap();
    assert_eq!(
        loaded.success_count, 2,
        "success_count must accumulate across two apply_delta upserts"
    );
    assert!(
        loaded.score > ServerTrustScore::INITIAL_SCORE,
        "score must reflect both positive deltas"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_and_apply_delta_upsert_increments_counters_on_postgres() {
    let (pool, _container) = start_pg().await;
    let store = TrustScoreStore::new(pool);
    store.init().await.unwrap();

    // First call inserts; the second hits the ON CONFLICT DO UPDATE branch, exercising the
    // same unqualified self-reference bug as apply_delta but in a sibling method.
    store
        .load_and_apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
        .await
        .unwrap();
    store
        .load_and_apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
        .await
        .unwrap();

    let loaded = store.load("srv1").await.unwrap().unwrap();
    assert_eq!(
        loaded.success_count, 2,
        "success_count must accumulate across two load_and_apply_delta upserts"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_all_decodes_int4_counts_on_postgres() {
    let (pool, _container) = start_pg().await;
    let store = TrustScoreStore::new(pool);
    store.init().await.unwrap();

    store.apply_delta("srv1", 0.02, 3, 1).await.unwrap();
    store.apply_delta("srv2", -0.10, 0, 2).await.unwrap();

    let all = store.load_all().await.unwrap();
    assert_eq!(all.len(), 2, "load_all must return both servers");

    let srv1 = all
        .iter()
        .find(|s| s.server_id == "srv1")
        .expect("srv1 must be present");
    assert_eq!(
        srv1.success_count, 3,
        "success_count must decode as the full INTEGER value, not error with ColumnDecode"
    );
    assert_eq!(srv1.failure_count, 1);

    let srv2 = all
        .iter()
        .find(|s| s.server_id == "srv2")
        .expect("srv2 must be present");
    assert_eq!(srv2.failure_count, 2);
}
