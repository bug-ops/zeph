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
//! Regression coverage for issue #5803: `TrustScoreStore::load_and_apply_delta` builds an
//! `INSERT ... ON CONFLICT(server_id) DO UPDATE SET` statement with self-references (e.g.
//! `success_count = success_count + excluded.success_count`). Postgres's `ON CONFLICT DO
//! UPDATE` always exposes an implicit `excluded` pseudo-table alongside the target table, so
//! an unqualified self-reference is rejected as ambiguous (`column reference "success_count"
//! is ambiguous`) — every self-reference in the generated SQL must be table-qualified
//! (`mcp_trust_scores.score`, not bare `score`). `SQLite` accepts the unqualified form, so the
//! existing `store_*` unit tests (`trust_score.rs`, in-memory `SqliteStore`) never caught it.
//! These tests exercise the upsert branch against a real Postgres instance.
//!
//! Also covers issue #6073: `load_and_apply_delta` was rewritten to compute asymmetric time
//! decay (see `ServerTrustScore::apply_decay`) directly inside the same atomic SQL statement,
//! using a `CASE WHEN` expression with a `CAST(... AS REAL)` division. `decay_atomic_on_postgres`
//! exercises that expression against real Postgres to catch any dialect-specific arithmetic or
//! cast mismatch that an in-memory `SQLite` test would not surface.
//!
//! Also covers a co-located defect found while adding this coverage: `load()`/`load_all()`
//! decoded `success_count`/`failure_count` as `i64`, but both columns are `INTEGER` (INT4)
//! in the Postgres schema (migration `052_mcp_trust_scores.sql`), which `sqlx-postgres`
//! rejects as `ColumnDecode`. `load_all_decodes_int4_counts_on_postgres` closes the gap for
//! `load_all()` specifically (the other tests already exercise `load()` via
//! `load_and_apply_delta`'s round trips).

#![cfg(feature = "test-utils")]

use std::time::Duration;

use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;
use zeph_db::DbConfig;
use zeph_db::sql;
use zeph_mcp::{ServerTrustScore, TrustScoreStore};

// Generous startup timeout, matching the zeph-memory pattern: under concurrent CI load the
// default 60s can elapse before Postgres is ready.
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
async fn decay_atomic_on_postgres() {
    let (pool, _container) = start_pg().await;
    let store = TrustScoreStore::new(pool.clone());
    store.init().await.unwrap();

    // Insert a high score with an old timestamp (simulate 30 days ago) directly, bypassing
    // the store API — this is the state `load_and_apply_delta`'s decay expression must read.
    let old_ts = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(30 * 86_400),
    )
    .unwrap();
    zeph_db::query(sql!(
        "INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
         VALUES (?, 0.9, 0, 0, ?)"
    ))
    .bind("srv1")
    .bind(old_ts)
    .execute(&pool)
    .await
    .unwrap();

    // Delta = 0.0 — exercises decay-only through the CASE WHEN / CAST(... AS REAL) expression
    // on real Postgres, where the `updated_at_secs` column is BIGINT (not SQLite's flexible
    // INTEGER affinity) — a dialect-specific arithmetic mismatch would surface here.
    store.load_and_apply_delta("srv1", 0.0, 0, 0).await.unwrap();

    let loaded = store.load("srv1").await.unwrap().unwrap();
    assert!(
        loaded.score < 0.9,
        "score should have decayed from 0.9, got {}",
        loaded.score
    );
    assert!(
        loaded.score >= ServerTrustScore::INITIAL_SCORE,
        "score should not decay below INITIAL_SCORE, got {}",
        loaded.score
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_all_decodes_int4_counts_on_postgres() {
    let (pool, _container) = start_pg().await;
    let store = TrustScoreStore::new(pool);
    store.init().await.unwrap();

    store
        .load_and_apply_delta("srv1", 0.02, 3, 1)
        .await
        .unwrap();
    store
        .load_and_apply_delta("srv2", -0.10, 0, 2)
        .await
        .unwrap();

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
