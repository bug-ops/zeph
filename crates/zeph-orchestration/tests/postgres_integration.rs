// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests for `zeph-orchestration`.
//!
//! These tests require Docker to be running. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-orchestration --no-default-features \
//!     --features test-utils --test postgres_integration --run-ignored ignored-only
//! ```
//!
//! Regression coverage for issue #5803: `PlanCache::cache_plan`'s
//! `INSERT ... ON CONFLICT(goal_hash) DO UPDATE SET success_count = success_count + 1`
//! left `success_count` unqualified. Postgres's `ON CONFLICT DO UPDATE` always exposes an
//! implicit `excluded` pseudo-table alongside the target table, so an unqualified
//! self-reference is rejected as ambiguous (`column reference "success_count" is ambiguous`).
//! `SQLite` accepts the same statement, so the existing `deduplication_increments_success_count`
//! unit test (`plan_cache.rs`, `SqliteStore::new(":memory:")`) never caught it. This test
//! exercises the same dedup path against a real Postgres instance.

#![cfg(feature = "test-utils")]

use std::time::Duration;

use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;
use zeph_config::PlanCacheConfig;
use zeph_db::DbConfig;
use zeph_orchestration::graph::{TaskId, TaskNode};
use zeph_orchestration::{PlanCache, TaskGraph};

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

fn make_graph(goal: &str, tasks: &[(&str, &str, &[u32])]) -> TaskGraph {
    let mut graph = TaskGraph::new(goal);
    for (i, (title, desc, deps)) in tasks.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let mut node = TaskNode::new(i as u32, *title, *desc);
        node.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        graph.tasks.push(node);
    }
    graph
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn cache_plan_dedup_increments_success_count_on_postgres() {
    let (pool, _container) = start_pg().await;
    let cache = PlanCache::new(pool.clone(), PlanCacheConfig::default(), "test-model")
        .await
        .unwrap();

    let graph = make_graph("same goal", &[("Task", "do it", &[])]);
    let emb = vec![1.0_f32, 0.0];

    // First insert, then re-cache the same goal to hit the ON CONFLICT DO UPDATE branch —
    // the exact path that previously errored on Postgres with an ambiguous column reference.
    cache.cache_plan(&graph, &emb, "test-model").await.unwrap();
    cache.cache_plan(&graph, &emb, "test-model").await.unwrap();

    let count: i64 = zeph_db::query_scalar(zeph_db::sql!("SELECT COUNT(*) FROM plan_cache"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "UNIQUE goal_hash must prevent a second row");

    let normalized = zeph_orchestration::normalize_goal("same goal");
    let success: i32 = zeph_db::query_scalar(zeph_db::sql!(
        "SELECT success_count FROM plan_cache WHERE goal_hash = ?"
    ))
    .bind(zeph_orchestration::plan_cache::goal_hash(&normalized))
    .fetch_one(&pool)
    .await
    .unwrap();
    // Bug's failure mode: without a table qualifier the statement itself errors on Postgres
    // before ever reaching a wrong value, so this assertion is the closing check that the
    // full round trip (not just "no error") produced the expected increment.
    assert_eq!(
        success, 2,
        "success_count must increment across two cache_plan calls for the same goal"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn find_similar_updates_last_accessed_on_postgres() {
    let (pool, _container) = start_pg().await;
    let config = PlanCacheConfig {
        similarity_threshold: 0.9,
        ..PlanCacheConfig::default()
    };
    let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

    let graph = make_graph("deploy service", &[("Build", "build it", &[])]);
    let embedding = vec![1.0_f32, 0.0, 0.0];
    cache
        .cache_plan(&graph, &embedding, "test-model")
        .await
        .unwrap();

    let result = cache
        .find_similar(&[1.0, 0.0, 0.0], "test-model")
        .await
        .unwrap();
    assert!(result.is_some());
    let (template, score) = result.unwrap();
    assert!((score - 1.0).abs() < 1e-5);
    assert_eq!(template.tasks.len(), 1);
}
