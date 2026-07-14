// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI integration coverage for `zeph durable prune` (#6254).
//!
//! `handle_durable_command`'s `Prune` branch only `println!`s its report — there is no return
//! value a unit test could assert on — so this drives the real compiled `zeph` binary as a
//! subprocess (mirroring `tests/daemon_boot.rs`'s `zeph_bin_path` pattern) and asserts on its
//! captured stdout. Seeds a real `durable.db` journal directly through `zeph_durable::LocalBackend`
//! at the exact path `resolve_durable_db_url` resolves for the test config's `memory.sqlite_path`,
//! so both the crash-orphan count and the TTL-prunable count are exercised against genuine rows,
//! not mocks.

use std::process::Command;

use zeph_durable::{ExecutionId, ExecutionKind, ExecutionStatus, Journal as _, LocalBackend};

/// Resolves the path to the built `zeph` binary at runtime — see `tests/daemon_boot.rs::zeph_bin_path`
/// for why this must be a runtime env var lookup rather than the `env!("CARGO_BIN_EXE_zeph")` macro.
fn zeph_bin_path() -> String {
    std::env::var("NEXTEST_BIN_EXE_zeph")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_zeph"))
        .expect(
            "NEXTEST_BIN_EXE_zeph or CARGO_BIN_EXE_zeph must be set by the test runner \
             (cargo test / cargo nextest run / cargo nextest run --archive-file)",
        )
}

async fn backdate_updated_at(backend: &LocalBackend, id: ExecutionId, updated_at_ms: i64) {
    zeph_db::query(zeph_db::sql!(
        "UPDATE durable_executions SET updated_at = ? WHERE execution_id = ?"
    ))
    .bind(updated_at_ms)
    .bind(id.as_uuid().to_string())
    .execute(backend.pool())
    .await
    .unwrap();
}

async fn backdate_finalized_at(backend: &LocalBackend, id: ExecutionId, finalized_at_ms: i64) {
    zeph_db::query(zeph_db::sql!(
        "UPDATE durable_executions SET finalized_at = ? WHERE execution_id = ?"
    ))
    .bind(finalized_at_ms)
    .bind(id.as_uuid().to_string())
    .execute(backend.pool())
    .await
    .unwrap();
}

/// `zeph durable prune --dry-run` must report the crash-orphan count and the TTL-prunable count
/// as two separate lines, and must not mutate anything (#6254's CLI wiring: `count_orphans` runs
/// before `count_prunable`, mirroring the non-dry-run `sweep_orphans`-before-`prune` ordering).
#[tokio::test]
async fn durable_prune_dry_run_reports_orphan_and_ttl_counts_separately() {
    let tmp = tempfile::tempdir().unwrap();
    let sqlite_path = tmp.path().join("zeph.db");

    let mut doc: toml_edit::DocumentMut = zeph_core::config::Config::dump_defaults()
        .expect("dump default config")
        .parse()
        .expect("parse default config toml");
    doc["memory"]["sqlite_path"] = toml_edit::value(sqlite_path.display().to_string());
    doc["vault"]["backend"] = toml_edit::value("env");
    doc["durable"]["retention"]["stale_running_after_secs"] = toml_edit::value(1_i64);
    doc["durable"]["retention"]["ttl_failed_secs"] = toml_edit::value(1_i64);
    let config_path = tmp.path().join("test.toml");
    std::fs::write(&config_path, doc.to_string()).expect("write test config");

    // Seed the durable journal at the exact URL `resolve_durable_db_url` resolves for this
    // `memory.sqlite_path` (no pre-existing legacy `durable.db`, so it's `<sqlite_path>.durable.db`).
    let durable_url = format!("{}.durable.db", sqlite_path.display());
    let backend = LocalBackend::open(&durable_url, 1_048_576)
        .await
        .expect("open seed backend");
    backend.init().await.expect("init durable schema");

    // One crash-orphaned `running` execution: stale `updated_at`, no lock held (nothing acquired
    // an `ExecutionLock` for it), simulating a crashed owner.
    let orphan = ExecutionId::new();
    backend
        .open_execution(orphan, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    backdate_updated_at(&backend, orphan, 0).await;

    // One terminal execution past its TTL.
    let stale_failed = ExecutionId::new();
    backend
        .open_execution(stale_failed, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    backend
        .finalize(stale_failed, ExecutionStatus::Failed)
        .await
        .unwrap();
    backdate_finalized_at(&backend, stale_failed, 0).await;

    // Release the pool's connections before the subprocess opens the same sqlite file.
    backend.pool().close().await;
    drop(backend);

    let bin = zeph_bin_path();
    let output = Command::new(&bin)
        .arg("--config")
        .arg(&config_path)
        .arg("durable")
        .arg("prune")
        .arg("--dry-run")
        .output()
        .expect("spawn zeph durable prune --dry-run");
    assert!(
        output.status.success(),
        "zeph durable prune --dry-run must exit successfully; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Dry run: 1 orphaned execution(s) would be aborted."),
        "expected a separate orphan-count line, got stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Dry run: 1 terminal execution(s) past TTL would be pruned."),
        "expected a separate TTL-prunable-count line, got stdout:\n{stdout}"
    );

    // A dry run must not have mutated anything.
    let verify_backend = LocalBackend::open(&durable_url, 1_048_576)
        .await
        .expect("reopen backend to verify no mutation");
    let (orphan_status,): (String,) = zeph_db::query_as(zeph_db::sql!(
        "SELECT status FROM durable_executions WHERE execution_id = ?"
    ))
    .bind(orphan.as_uuid().to_string())
    .fetch_one(verify_backend.pool())
    .await
    .unwrap();
    assert_eq!(
        orphan_status, "running",
        "dry-run must not abort the orphan"
    );

    let (failed_still_present,): (i64,) = zeph_db::query_as(zeph_db::sql!(
        "SELECT COUNT(*) FROM durable_executions WHERE execution_id = ?"
    ))
    .bind(stale_failed.as_uuid().to_string())
    .fetch_one(verify_backend.pool())
    .await
    .unwrap();
    assert_eq!(
        failed_still_present, 1,
        "dry-run must not delete the prunable row"
    );
}

/// The non-dry-run `zeph durable prune` must actually sweep the orphan and prune the terminal
/// row, reporting both counts on their own lines.
#[tokio::test]
async fn durable_prune_without_dry_run_sweeps_and_prunes_and_reports_both_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let sqlite_path = tmp.path().join("zeph.db");

    let mut doc: toml_edit::DocumentMut = zeph_core::config::Config::dump_defaults()
        .expect("dump default config")
        .parse()
        .expect("parse default config toml");
    doc["memory"]["sqlite_path"] = toml_edit::value(sqlite_path.display().to_string());
    doc["vault"]["backend"] = toml_edit::value("env");
    doc["durable"]["retention"]["stale_running_after_secs"] = toml_edit::value(1_i64);
    doc["durable"]["retention"]["ttl_failed_secs"] = toml_edit::value(1_i64);
    let config_path = tmp.path().join("test.toml");
    std::fs::write(&config_path, doc.to_string()).expect("write test config");

    let durable_url = format!("{}.durable.db", sqlite_path.display());
    let backend = LocalBackend::open(&durable_url, 1_048_576)
        .await
        .expect("open seed backend");
    backend.init().await.expect("init durable schema");

    let orphan = ExecutionId::new();
    backend
        .open_execution(orphan, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    backdate_updated_at(&backend, orphan, 0).await;

    let stale_failed = ExecutionId::new();
    backend
        .open_execution(stale_failed, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    backend
        .finalize(stale_failed, ExecutionStatus::Failed)
        .await
        .unwrap();
    backdate_finalized_at(&backend, stale_failed, 0).await;

    backend.pool().close().await;
    drop(backend);

    let bin = zeph_bin_path();
    let output = Command::new(&bin)
        .arg("--config")
        .arg(&config_path)
        .arg("durable")
        .arg("prune")
        .output()
        .expect("spawn zeph durable prune");
    assert!(
        output.status.success(),
        "zeph durable prune must exit successfully; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Aborted 1 orphaned execution(s)."),
        "expected a separate sweep-count line, got stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Pruned 1 execution(s)."),
        "expected a separate prune-count line, got stdout:\n{stdout}"
    );

    let verify_backend = LocalBackend::open(&durable_url, 1_048_576)
        .await
        .expect("reopen backend to verify mutation");
    let (orphan_status,): (String,) = zeph_db::query_as(zeph_db::sql!(
        "SELECT status FROM durable_executions WHERE execution_id = ?"
    ))
    .bind(orphan.as_uuid().to_string())
    .fetch_one(verify_backend.pool())
    .await
    .unwrap();
    assert_eq!(orphan_status, "aborted", "the orphan must have been swept");

    let (failed_still_present,): (i64,) = zeph_db::query_as(zeph_db::sql!(
        "SELECT COUNT(*) FROM durable_executions WHERE execution_id = ?"
    ))
    .bind(stale_failed.as_uuid().to_string())
    .fetch_one(verify_backend.pool())
    .await
    .unwrap();
    assert_eq!(
        failed_still_present, 0,
        "the stale failed execution must have been pruned (deleted)"
    );
}
