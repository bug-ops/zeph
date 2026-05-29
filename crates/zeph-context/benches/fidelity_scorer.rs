// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Benchmark: AC-11 — `score_and_apply` on 500 synthetic messages must complete <2ms.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use zeph_common::memory::TokenCounting;
use zeph_context::fidelity::{FidelityConfig, FidelityScorer};
use zeph_llm::provider::{Message, MessageMetadata, Role};

struct CharDivTc(usize);
impl TokenCounting for CharDivTc {
    fn count_tokens(&self, text: &str) -> usize {
        text.len() / self.0.max(1)
    }
    fn count_tool_schema_tokens(&self, _: &serde_json::Value) -> usize {
        0
    }
}

fn make_synthetic_messages(n: usize) -> Vec<Message> {
    (0..n)
        .map(|i| {
            let role = match i % 3 {
                0 => Role::System,
                1 => Role::User,
                _ => Role::Assistant,
            };
            Message {
                role,
                content: format!(
                    "synthetic message content number {i} with some extra words for scoring"
                ),
                parts: vec![],
                metadata: MessageMetadata::default(),
            }
        })
        .collect()
}

fn bench_score_500(c: &mut Criterion) {
    let scorer = FidelityScorer;
    let cfg = FidelityConfig {
        enabled: true,
        w_temporal: 0.3,
        w_importance: 0.2,
        w_semantic: 0.3,
        w_plan: 0.2,
        full_threshold: 0.7,
        compressed_threshold: 0.3,
        compressed_max_tokens: 50,
        regrade_threshold: 0.6,
        min_query_length: 5,
        max_scored_messages: 500,
        exempt_tail_messages: 0,
        compress_provider: None,
        semantic_scoring_provider: None,
        lookahead_depth: 3,
        embed_concurrency: 32,
        max_embed_input_tokens: None,
        max_compress_input_tokens: None,
    };
    let tc = CharDivTc(4);
    let base_messages = make_synthetic_messages(500);

    c.bench_function("fidelity_score_and_apply_500", |b| {
        b.iter(|| {
            let mut messages = base_messages.clone();
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            rt.block_on(scorer.score_and_apply(
                black_box(&mut messages),
                black_box("query words for semantic signal"),
                black_box(&[]),
                black_box(&cfg),
                black_box(&tc),
                black_box(0),
                black_box(false),
                black_box(None),
                black_box(None),
            ));
        });
    });
}

criterion_group!(benches, bench_score_500);
criterion_main!(benches);
