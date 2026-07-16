// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Criterion benchmarks for `zeph-durable` — all 10 groups from spec-064 §Benchmarks.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use futures::future;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use zeph_durable::cipher::CipherError;
use zeph_durable::journal::Journal as _;
use zeph_durable::{
    DurableBackendEnum, DurableConfig, DurableContext, EffectIntentSubClass, EntryKindTag,
    ExecutionId, ExecutionKind, ExecutionStatus, IdempotencyKey, JournalWriter,
    JournalWriterHandle, LocalBackend, OnAmbiguous, PayloadAad, PayloadCipher, RetentionPolicy,
    StepDescriptor, StepId,
};

// ── cipher stub ─────────────────────────────────────────────────────────────

/// A real XChaCha20-Poly1305 cipher backed by `chacha20poly1305`.
///
/// The key is fixed for bench isolation (not production use). INV-1: this lives in dev-dependencies
/// only and is never compiled into the runtime binary.
struct BenchCipher {
    inner: XChaCha20Poly1305,
}

impl BenchCipher {
    fn new() -> Self {
        // 32 zero bytes — deterministic, bench-only key.
        let key = chacha20poly1305::Key::from([0u8; 32]);
        Self {
            inner: XChaCha20Poly1305::new(&key),
        }
    }
}

impl PayloadCipher for BenchCipher {
    fn seal(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
        // key_id(1) || nonce(24) || ciphertext || tag(16)
        let nonce_bytes: [u8; 24] = rand::random();
        let nonce = XNonce::from(nonce_bytes);
        let mut ciphertext = self
            .inner
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: plaintext,
                    aad: &aad.canonical_bytes(),
                },
            )
            .map_err(|_| CipherError::Authentication)?;
        let mut blob = Vec::with_capacity(1 + 24 + ciphertext.len());
        blob.push(0u8); // key_id
        blob.extend_from_slice(&nonce_bytes);
        blob.append(&mut ciphertext);
        Ok(blob)
    }

    fn open(&self, sealed: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
        if sealed.len() < 25 {
            return Err(CipherError::Malformed {
                context: "blob shorter than key_id + nonce",
            });
        }
        let key_id = sealed[0];
        if key_id != 0 {
            return Err(CipherError::UnknownKeyId { key_id });
        }
        let nonce = XNonce::try_from(&sealed[1..25]).map_err(|_| CipherError::Malformed {
            context: "nonce slice is not exactly 24 bytes",
        })?;
        self.inner
            .decrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &sealed[25..],
                    aad: &aad.canonical_bytes(),
                },
            )
            .map_err(|_| CipherError::Authentication)
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

fn bench_config() -> DurableConfig {
    DurableConfig {
        journal_flush_interval_ms: 5,
        journal_ack_timeout_ms: 2000,
        ..DurableConfig::default()
    }
}

struct Harness {
    ctx: DurableContext,
    backend: Arc<LocalBackend>,
    writer_task: JoinHandle<()>,
    handle: JournalWriterHandle,
}

impl Harness {
    async fn open(exec: ExecutionId, is_resume: bool) -> Self {
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
            is_resume,
            backend,
            handle.clone(),
            &bench_config(),
        );
        Self {
            ctx,
            backend: local,
            writer_task,
            handle,
        }
    }

    fn resume_ctx(&self) -> DurableContext {
        let backend = Arc::new(DurableBackendEnum::Local(self.backend.clone()));
        DurableContext::new(
            self.ctx.execution_id(),
            ExecutionKind::AgentTurn,
            true,
            backend,
            self.handle.clone(),
            &bench_config(),
        )
    }

    async fn shutdown(self) {
        self.writer_task.abort();
        let _ = self.writer_task.await;
    }
}

fn rt() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

// ── bench 1: bench_step_run_idempotent ───────────────────────────────────────

fn bench_step_run_idempotent(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("bench_step_run_idempotent");

    // fresh arm
    group.bench_function(BenchmarkId::new("mode", "fresh"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                let v: u32 = h
                    .ctx
                    .step(
                        StepDescriptor::idempotent("bench_step", b"bench:idempotent".to_vec()),
                        |_| async { Ok::<u32, zeph_durable::StepError>(black_box(42)) },
                    )
                    .await
                    .unwrap();
                h.handle.flush().await.unwrap();
                h.shutdown().await;
                black_box(v)
            })
        });
    });

    // replay arm
    group.bench_function(BenchmarkId::new("mode", "replay"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                let desc =
                    || StepDescriptor::idempotent("bench_step", b"bench:idempotent".to_vec());
                let _: u32 = h.ctx.step(desc(), |_| async { Ok(42) }).await.unwrap();
                h.handle.flush().await.unwrap();
                let resumed = h.resume_ctx();
                let v: u32 = resumed.step(desc(), |_| async { Ok(999) }).await.unwrap();
                h.shutdown().await;
                black_box(v)
            })
        });
    });

    group.finish();
}

// ── bench 2: bench_step_run_atleastonce ──────────────────────────────────────

fn bench_step_run_atleastonce(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("bench_step_run_atleastonce");

    group.bench_function(BenchmarkId::new("mode", "fresh"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                let v: u32 = h
                    .ctx
                    .step(
                        StepDescriptor::at_least_once("bench_alo", b"bench:atleastonce".to_vec()),
                        |_| async { Ok::<u32, zeph_durable::StepError>(black_box(1)) },
                    )
                    .await
                    .unwrap();
                h.handle.flush().await.unwrap();
                h.shutdown().await;
                black_box(v)
            })
        });
    });

    group.bench_function(BenchmarkId::new("mode", "replay"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                let desc =
                    || StepDescriptor::at_least_once("bench_alo", b"bench:atleastonce".to_vec());
                let _: u32 = h.ctx.step(desc(), |_| async { Ok(1) }).await.unwrap();
                h.handle.flush().await.unwrap();
                let resumed = h.resume_ctx();
                let v: u32 = resumed.step(desc(), |_| async { Ok(999) }).await.unwrap();
                h.shutdown().await;
                black_box(v)
            })
        });
    });

    group.finish();
}

// ── bench 3: bench_step_run_exactly_once_n (N=5) ─────────────────────────────

fn bench_step_run_exactly_once_n(c: &mut Criterion) {
    const N: usize = 5;
    let rt = rt();
    let mut group = c.benchmark_group("bench_step_run_exactly_once_n");

    group.bench_function(BenchmarkId::new("n", N), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                for i in 0..N {
                    let fp = format!("bench:exactly_once:{i}").into_bytes();
                    let desc = StepDescriptor::exactly_once_guarded(
                        "bench_eo",
                        EffectIntentSubClass::Destructive,
                        Some(OnAmbiguous::Fail),
                        fp,
                    )
                    .unwrap();
                    let v = u32::try_from(i).unwrap_or(u32::MAX);
                    let _: u32 = h
                        .ctx
                        .step(desc, |_| async move {
                            Ok::<u32, zeph_durable::StepError>(black_box(v))
                        })
                        .await
                        .unwrap();
                }
                h.handle.flush().await.unwrap();
                h.shutdown().await;
            });
        });
    });

    group.finish();
}

// ── bench 4: bench_parallel_n (N=8) ──────────────────────────────────────────

fn bench_parallel_n(c: &mut Criterion) {
    const N: usize = 8;
    let rt = rt();
    let mut group = c.benchmark_group("bench_parallel_n");

    group.bench_function(BenchmarkId::new("n", N), |b| {
        b.iter(|| {
            rt.block_on(async {
                let exec = ExecutionId::new();
                let h = Harness::open(exec, false).await;
                let scope = h.ctx.parallel();
                // Ids are assigned eagerly, in construction order, before any future is polled.
                let futs: Vec<_> = (0..N)
                    .map(|i| {
                        let fp = format!("bench:parallel:{i}").into_bytes();
                        let desc = StepDescriptor::idempotent("bench_par", fp);
                        let v = u32::try_from(i).unwrap_or(u32::MAX);
                        scope.step(desc, move |_| async move {
                            Ok::<u32, zeph_durable::StepError>(black_box(v))
                        })
                    })
                    .collect();
                let results = future::join_all(futs).await;
                h.handle.flush().await.unwrap();
                h.shutdown().await;
                black_box(results)
            })
        });
    });

    group.finish();
}

// ── bench 5: bench_replay_cursor_n (N=5000) ──────────────────────────────────

fn bench_replay_cursor_n(c: &mut Criterion) {
    const N: usize = 5000;
    let rt = rt();
    let mut group = c.benchmark_group("bench_replay_cursor_n");

    group.bench_function(BenchmarkId::new("steps", N), |b| {
        // Seed 5000 step results once, outside the measured loop.
        let (exec, backend, writer_task, handle) = rt.block_on(async {
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
            // Write N idempotent steps to seed the journal.
            for i in 0..N {
                let fp = format!("bench:replay:{i}").into_bytes();
                let v = u32::try_from(i).unwrap_or(u32::MAX);
                let _: u32 = ctx
                    .step(
                        StepDescriptor::idempotent("seed", fp),
                        move |_| async move { Ok::<u32, zeph_durable::StepError>(v) },
                    )
                    .await
                    .unwrap();
            }
            handle.flush().await.unwrap();
            (exec, local, writer_task, handle)
        });

        b.iter(|| {
            rt.block_on(async {
                // Resume from the fully-seeded journal, replaying all N steps.
                let backend_enum = Arc::new(DurableBackendEnum::Local(backend.clone()));
                let ctx = DurableContext::new(
                    exec,
                    ExecutionKind::AgentTurn,
                    true,
                    backend_enum,
                    handle.clone(),
                    &bench_config(),
                );
                for i in 0..N {
                    let fp = format!("bench:replay:{i}").into_bytes();
                    let v: u32 = ctx
                        .step(StepDescriptor::idempotent("seed", fp), |_| async {
                            Ok::<u32, zeph_durable::StepError>(999)
                        })
                        .await
                        .unwrap();
                    black_box(v);
                }
            });
        });

        // Cleanup after bench group.
        rt.block_on(async {
            writer_task.abort();
            let _ = writer_task.await;
        });
    });

    group.finish();
}

// ── bench 6: bench_journal_append_buffered ────────────────────────────────────

fn bench_journal_append_buffered(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("bench_journal_append_buffered");

    let (exec, handle, _backend, writer_task) = rt.block_on(async {
        let exec = ExecutionId::new();
        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
        let writer_task = tokio::spawn(writer.run());
        (exec, handle, local, writer_task)
    });

    let step_counter = AtomicU32::new(0);
    group.bench_function("append_buffered", |b| {
        b.iter(|| {
            // Fire-and-forget: measure only the channel send, not the flush.
            use bytes::Bytes;
            use zeph_durable::effect::EffectClass;
            use zeph_durable::journal::{EntryKind, JournalEntry};
            let step_id = StepId::new(step_counter.fetch_add(1, Ordering::Relaxed));
            let ikey = IdempotencyKey::derive(exec, step_id, b"bench:buffered");
            let entry = JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id,
                entry: EntryKind::StepResult {
                    idempotency_key: ikey,
                    payload: Bytes::from_static(b"bench"),
                    effect: EffectClass::Idempotent,
                    payload_version: 1,
                },
                created_at_ms: 0,
            };
            handle.append_buffered(black_box(entry));
        });
    });

    rt.block_on(async {
        writer_task.abort();
        let _ = writer_task.await;
    });

    group.finish();
}

// ── bench 7: bench_journal_append_acked ───────────────────────────────────────

fn bench_journal_append_acked(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("bench_journal_append_acked");

    let (exec, handle, _backend, writer_task) = rt.block_on(async {
        let exec = ExecutionId::new();
        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
        let writer_task = tokio::spawn(writer.run());
        (exec, handle, local, writer_task)
    });

    let step_counter = AtomicU32::new(0);
    group.bench_function("append_acked", |b| {
        b.iter(|| {
            rt.block_on(async {
                use bytes::Bytes;
                use zeph_durable::effect::EffectClass;
                use zeph_durable::journal::{EntryKind, JournalEntry};
                let step_id = StepId::new(step_counter.fetch_add(1, Ordering::Relaxed));
                let ikey = IdempotencyKey::derive(exec, step_id, b"bench:acked");
                let entry = JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: ExecutionKind::AgentTurn,
                    step_id,
                    entry: EntryKind::StepResult {
                        idempotency_key: ikey,
                        payload: Bytes::from_static(b"bench"),
                        effect: EffectClass::Idempotent,
                        payload_version: 1,
                    },
                    created_at_ms: 0,
                };
                let seq = handle.append_acked(black_box(entry)).await.unwrap();
                black_box(seq)
            })
        });
    });

    rt.block_on(async {
        writer_task.abort();
        let _ = writer_task.await;
    });

    group.finish();
}

// ── bench 8: bench_payload_seal (4 KiB) ──────────────────────────────────────

fn bench_payload_seal(c: &mut Criterion) {
    let cipher = BenchCipher::new();
    let exec = ExecutionId::new();
    let aad = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, None);
    let plaintext = vec![0u8; 4096];

    let mut group = c.benchmark_group("bench_payload_seal");
    group.bench_function("4kib", |b| {
        b.iter(|| {
            let blob = cipher.seal(black_box(&plaintext), black_box(&aad)).unwrap();
            black_box(blob)
        });
    });
    group.finish();
}

// ── bench 9: bench_payload_open (4 KiB) ──────────────────────────────────────

fn bench_payload_open(c: &mut Criterion) {
    let cipher = BenchCipher::new();
    let exec = ExecutionId::new();
    let aad = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, None);
    let plaintext = vec![0u8; 4096];
    // Seal once outside the timed loop.
    let blob = cipher.seal(&plaintext, &aad).unwrap();

    let mut group = c.benchmark_group("bench_payload_open");
    group.bench_function("4kib", |b| {
        b.iter(|| {
            let plain = cipher.open(black_box(&blob), black_box(&aad)).unwrap();
            black_box(plain)
        });
    });
    group.finish();
}

// ── bench 10: bench_prune_batch (10000 entries, 500-row batches) ──────────────

fn bench_prune_batch(c: &mut Criterion) {
    use criterion::BatchSize;

    const TOTAL: usize = 10_000;
    let rt = rt();
    let policy = RetentionPolicy {
        ttl_completed_secs: 0,
        ttl_failed_secs: 0,
        prune_batch_size: 500,
        ..RetentionPolicy::default()
    };
    let mut group = c.benchmark_group("bench_prune_batch");

    // iter_batched: setup (seed 10 000 entries) runs once per batch outside the timer;
    // only the prune call itself is measured.
    group.bench_function("10000_entries", |b| {
        b.iter_batched(
            || {
                // Setup: build a fresh backend with TOTAL finalized executions.
                rt.block_on(async {
                    let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
                    local.init().await.unwrap();
                    let (writer, handle) = JournalWriter::new(local.clone(), &bench_config());
                    let writer_task = tokio::spawn(writer.run());
                    for _ in 0..TOTAL {
                        let exec = ExecutionId::new();
                        local
                            .open_execution(exec, ExecutionKind::AgentTurn)
                            .await
                            .unwrap();
                        local
                            .finalize(exec, ExecutionStatus::Completed)
                            .await
                            .unwrap();
                    }
                    writer_task.abort();
                    let _ = writer_task.await;
                    drop(handle);
                    Arc::new(DurableBackendEnum::Local(local))
                })
            },
            |backend| {
                // Measured: prune only.
                rt.block_on(async {
                    let deleted = backend.prune(&policy).await.unwrap();
                    black_box(deleted)
                })
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── entry points ──────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_step_run_idempotent,
    bench_step_run_atleastonce,
    bench_step_run_exactly_once_n,
    bench_parallel_n,
    bench_replay_cursor_n,
    bench_journal_append_buffered,
    bench_journal_append_acked,
    bench_payload_seal,
    bench_payload_open,
    bench_prune_batch,
);
criterion_main!(benches);
