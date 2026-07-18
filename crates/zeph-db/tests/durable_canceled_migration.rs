// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression test for migration 112 (#6362, critic finding F1): the `durable_executions` table
//! rebuild (adding `'canceled'` to the `SQLite` CHECK constraint) must preserve every FK-linked
//! child row in `durable_journal`/`durable_promises`/`durable_timers`.
//!
//! Precedent migration 054 is a false precedent for this rebuild pattern — it rebuilds a table
//! with zero inbound foreign keys, so it never exercises the parent-drop path. `durable_executions`
//! is the parent of three FK-children (all default `NO ACTION`), and `PRAGMA foreign_keys = OFF`
//! is a documented `SQLite` no-op *inside a transaction* — sqlx wraps ordinary migrations in one, so
//! a migration that forgot the `-- no-transaction` marker would pass this exact test against an
//! empty database and then FK-violate in production the instant a child row exists. This test
//! seeds all three child tables before running migration 112 through the real production pool
//! config (`.foreign_keys(true)`, matching `crates/zeph-db/src/pool.rs`), which is what actually
//! exposes that failure mode.

#![cfg(all(feature = "sqlite", not(feature = "postgres")))]

use std::str::FromStr as _;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn migration_112_preserves_fk_linked_child_rows_across_the_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("durable.db");

    // Mirrors the real production pool config (`connect_sqlite` in `src/pool.rs`): a file-backed
    // database with foreign key enforcement on. A `:memory:` or FK-off connection would not
    // exercise the bug this test guards against.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}?mode=rwc", db_path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("failed to open file-backed sqlite pool");

    let migrator = sqlx::migrate!("./migrations/sqlite");

    // Apply every migration through 111 — durable_executions/journal/promises/timers all exist,
    // but the 112 rebuild has not run yet, leaving a window to seed FK-populated data.
    migrator
        .run_to(111, &pool)
        .await
        .expect("migrations through 111 must apply cleanly");

    let exec_id = "11111111-1111-1111-1111-111111111111";
    sqlx::query(
        "INSERT INTO durable_executions (execution_id, kind, status, created_at, updated_at, finalized_at)
         VALUES (?, 'agent_turn', 'running', 0, 0, NULL)",
    )
    .bind(exec_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO durable_journal (execution_id, step_id, entry_kind, created_at)
         VALUES (?, 0, 'test_entry', 0)",
    )
    .bind(exec_id)
    .execute(&pool)
    .await
    .unwrap();

    let promise_id = "22222222-2222-2222-2222-222222222222";
    sqlx::query(
        "INSERT INTO durable_promises (promise_id, execution_id, resolver_token_hash, created_at)
         VALUES (?, ?, ?, 0)",
    )
    .bind(promise_id)
    .bind(exec_id)
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await
    .unwrap();

    let timer_id = "33333333-3333-3333-3333-333333333333";
    sqlx::query(
        "INSERT INTO durable_timers (timer_id, execution_id, due_at, created_at)
         VALUES (?, ?, 0, 0)",
    )
    .bind(timer_id)
    .bind(exec_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the remaining migrations (112). Under the false-precedent 054 approach this would
    // FK-violate on the implicit `DROP TABLE durable_executions` cascading to its children.
    migrator
        .run(&pool)
        .await
        .expect("migration 112 must succeed with FK-populated child rows present");

    let (exec_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM durable_executions WHERE execution_id = ?")
            .bind(exec_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        exec_count, 1,
        "durable_executions row must survive the rebuild"
    );

    let (journal_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM durable_journal WHERE execution_id = ?")
            .bind(exec_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        journal_count, 1,
        "durable_journal child row must survive with its FK intact"
    );

    let (promise_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM durable_promises WHERE execution_id = ?")
            .bind(exec_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        promise_count, 1,
        "durable_promises child row must survive with its FK intact"
    );

    let (timer_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM durable_timers WHERE execution_id = ?")
            .bind(exec_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        timer_count, 1,
        "durable_timers child row must survive with its FK intact"
    );

    // The rebuilt CHECK constraint must now accept 'canceled' — the whole point of the migration.
    sqlx::query("UPDATE durable_executions SET status = 'canceled' WHERE execution_id = ?")
        .bind(exec_id)
        .execute(&pool)
        .await
        .expect("'canceled' must be accepted by the rebuilt CHECK constraint");
}
