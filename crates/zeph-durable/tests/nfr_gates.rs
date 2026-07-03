// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! NFR gate tests for zeph-durable (spec-064 §NFR-DE-01, NFR-DE-02, NFR-DE-07).
//!
//! These run inside the standard nextest suite (no criterion timing needed in CI). Each gate
//! collects wall-clock samples, discards a warm-up prefix, computes a percentile, and asserts it
//! against the spec bound. If a gate proves flaky on a noisy CI runner, mark it `#[ignore]` and
//! document in the PR — do NOT loosen the spec bounds.
//!
//! Every gate opens a real `LocalBackend` pool via `:memory:`, which is SQLite-specific (mirroring
//! the `src/`-side `with_backend` test modules): under `--features postgres`, `DbConfig::connect()`
//! takes cfg-priority and routes `:memory:` into `connect_postgres`, which fails to parse it as a
//! Postgres URL. See #5603.
#![cfg(all(feature = "sqlite", not(feature = "postgres")))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use zeph_durable::{
    DurableBackendEnum, DurableConfig, DurableContext, EffectIntentSubClass, ExecutionId,
    ExecutionKind, JournalWriter, LocalBackend, OnAmbiguous, StepDescriptor,
};

fn bench_config() -> DurableConfig {
    DurableConfig {
        journal_flush_interval_ms: 5,
        journal_ack_timeout_ms: 2000,
        ..DurableConfig::default()
    }
}

/// Compute the p99 of a sample vector (sorted ascending).
fn p99(mut samples: Vec<Duration>) -> Duration {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let len = samples.len();
    // 99th-percentile index: smallest i such that at least 99% of samples are ≤ samples[i].
    let p99_idx = ((len * 99).div_ceil(100)).min(len) - 1;
    samples[p99_idx]
}

// ── NFR-DE-01 ─────────────────────────────────────────────────────────────────

/// NFR-DE-01: N=5 `ExactlyOnceGuarded` end-to-end ≤ 5 ms p99.
///
/// 200 samples, first 10 discarded as warm-up. Uses a multi-thread runtime because the acked
/// writer is a spawned task — a `current_thread` runtime would deadlock.
///
/// Marked `#[ignore]` because the 5 ms bound is only achievable in release builds; debug-mode
/// `SQLite` I/O typically exceeds it by 5-10×. Run with `-- --ignored` in the nightly perf job or
/// `cargo test --release -p zeph-durable --test nfr_gates`. Do NOT loosen the 5 ms bound
/// (NFR-DE-01 is normative per spec-064).
#[ignore = "NFR-DE-01 bound only holds in release builds; run --ignored in nightly perf job"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_de_01_exactly_once_n5_p99_le_5ms() {
    const N: usize = 5;
    const SAMPLES: usize = 200;
    const WARMUP: usize = 10;

    let mut durations = Vec::with_capacity(SAMPLES);

    for _ in 0..SAMPLES {
        let exec = ExecutionId::new();
        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
        let writer_task = tokio::spawn(writer.run());
        let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
        let ctx = DurableContext::new(
            exec,
            ExecutionKind::AgentTurn,
            false,
            backend,
            handle.clone(),
            &bench_config(),
        );

        let start = Instant::now();
        for i in 0..N {
            let fp = format!("nfr01:eo:{i}").into_bytes();
            let desc = StepDescriptor::exactly_once_guarded(
                "nfr_de_01",
                EffectIntentSubClass::Destructive,
                Some(OnAmbiguous::Fail),
                fp,
            )
            .unwrap();
            let _: u32 = ctx
                .step(desc, |_| async {
                    Ok::<u32, zeph_durable::StepError>(u32::try_from(i).unwrap_or(u32::MAX))
                })
                .await
                .unwrap();
        }
        handle.flush().await.unwrap();
        durations.push(start.elapsed());

        writer_task.abort();
        let _ = writer_task.await;
    }

    // Discard warm-up.
    let measured: Vec<Duration> = durations.into_iter().skip(WARMUP).collect();
    let gate = p99(measured);
    assert!(
        gate <= Duration::from_millis(5),
        "NFR-DE-01 violated: p99={gate:?}, bound=5ms"
    );
}

// ── NFR-DE-02 ─────────────────────────────────────────────────────────────────

/// NFR-DE-02: range-cursor resume of a 5000-step synthetic journal ≤ 5 ms total.
///
/// Seeds the journal once (outside timing), then measures a single full replay pass.
///
/// Marked `#[ignore]` because the bound (5 ms for 5000 `SQLite` reads) is only achievable in
/// release builds with sccache. Run with `cargo nextest run --test nfr_gates -- --ignored` or in
/// the nightly perf job. Do NOT loosen the 5 ms bound (NFR-DE-02 is normative).
#[ignore = "NFR-DE-02 bound only holds in release builds; run --ignored in nightly perf job"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_de_02_replay_cursor_5000_steps_le_5ms() {
    const N: usize = 5000;

    let exec = ExecutionId::new();
    let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
    local.init().await.unwrap();
    local
        .open_execution(exec, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
    let writer_task = tokio::spawn(writer.run());
    let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
    let ctx = DurableContext::new(
        exec,
        ExecutionKind::AgentTurn,
        false,
        backend.clone(),
        handle.clone(),
        &bench_config(),
    );

    // Seed N idempotent steps.
    for i in 0..N {
        let fp = format!("nfr02:replay:{i}").into_bytes();
        let _: u32 = ctx
            .step(StepDescriptor::idempotent("seed", fp), |_| async {
                Ok::<u32, zeph_durable::StepError>(u32::try_from(i).unwrap_or(u32::MAX))
            })
            .await
            .unwrap();
    }
    handle.flush().await.unwrap();

    // Now measure the replay pass.
    let start = Instant::now();
    let resume_ctx = DurableContext::new(
        exec,
        ExecutionKind::AgentTurn,
        true,
        backend,
        handle.clone(),
        &bench_config(),
    );
    for i in 0..N {
        let fp = format!("nfr02:replay:{i}").into_bytes();
        let _: u32 = resume_ctx
            .step(StepDescriptor::idempotent("seed", fp), |_| async {
                Ok::<u32, zeph_durable::StepError>(999)
            })
            .await
            .unwrap();
    }
    let elapsed = start.elapsed();

    writer_task.abort();
    let _ = writer_task.await;

    assert!(
        elapsed <= Duration::from_millis(5),
        "NFR-DE-02 violated: replay of {N} steps took {elapsed:?}, bound=5ms"
    );
}

// ── NFR-DE-07 ─────────────────────────────────────────────────────────────────

/// NFR-DE-07: append throughput ≥ 1000 entries/s.
///
/// Measures 1000 buffered appends, verifies total time ≤ 1 s (= ≥ 1000 entries/s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_de_07_append_throughput_ge_1000_per_sec() {
    use bytes::Bytes;
    use zeph_durable::IdempotencyKey;
    use zeph_durable::StepId;
    use zeph_durable::effect::EffectClass;
    use zeph_durable::journal::{EntryKind, JournalEntry};

    const ENTRIES: usize = 1000;

    let exec = ExecutionId::new();
    let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
    local.init().await.unwrap();
    local
        .open_execution(exec, ExecutionKind::AgentTurn)
        .await
        .unwrap();
    let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
    let writer_task = tokio::spawn(writer.run());

    let start = Instant::now();
    for i in 0..u32::try_from(ENTRIES).unwrap_or(u32::MAX) {
        let step_id = StepId::new(i);
        let ikey = IdempotencyKey::derive(exec, step_id, b"nfr07:throughput");
        let entry = JournalEntry {
            seq: None,
            execution_id: exec,
            kind: ExecutionKind::AgentTurn,
            step_id,
            entry: EntryKind::StepResult {
                idempotency_key: ikey,
                payload: Bytes::from_static(b"bench_payload"),
                effect: EffectClass::Idempotent,
                payload_version: 1,
            },
            created_at_ms: 0,
        };
        handle.append_buffered(entry);
    }
    handle.flush().await.unwrap();
    let elapsed = start.elapsed();

    writer_task.abort();
    let _ = writer_task.await;

    assert!(
        elapsed <= Duration::from_secs(1),
        "NFR-DE-07 violated: {ENTRIES} buffered appends took {elapsed:?}, bound=1s (≥1000/s)"
    );
}
