// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Criterion benchmark stub for `InferenceWorker` channel throughput (#2818).
//!
//! Measures the overhead of the worker channel dispatch path (send + oneshot recv)
//! in isolation from model forward-pass latency.
//!
//! # Running
//!
//! ```
//! cargo bench --features candle -p zeph-llm --bench candle_inference
//! ```
//!
//! # TODO
//!
//! Replace the no-op worker body with a real `generate_tokens` call once a
//! lightweight model fixture is available for CI (e.g. a 1-layer GGUF stub).
//! Until then this bench validates the channel plumbing overhead only.

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_candle_worker_channel(c: &mut Criterion) {
    use tokio::runtime::Runtime;
    use zeph_llm::candle_provider::worker::InferenceRequest;

    let rt = Runtime::new().expect("tokio runtime");

    // Spawn a no-op worker: immediately echo Ok back through the oneshot.
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<InferenceRequest>(4);
    std::thread::spawn(move || {
        while let Some(req) = req_rx.blocking_recv() {
            let _ = req
                .reply
                .send(Ok(zeph_llm::candle_provider::generate::GenerationOutput {
                    text: String::new(),
                    tokens_generated: 0,
                }));
        }
    });

    c.bench_function("candle_worker_channel_roundtrip", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                req_tx
                    .send(InferenceRequest {
                        messages: vec![],
                        reply: reply_tx,
                    })
                    .await
                    .unwrap();
                reply_rx.await.unwrap().unwrap();
            });
        });
    });
}

criterion_group!(benches, bench_candle_worker_channel);
criterion_main!(benches);
