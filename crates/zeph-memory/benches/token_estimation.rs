use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use zeph_memory::estimate_tokens;

fn generate_text(size: usize) -> String {
    let paragraph = "The quick brown fox jumps over the lazy dog. \
                     This sentence contains various English words and punctuation marks.\n";
    paragraph.repeat(size / paragraph.len() + 1)[..size].to_string()
}

fn token_estimation(c: &mut Criterion) {
    let mut group = c.benchmark_group("estimate_tokens");

    for size in [1_000, 10_000, 100_000] {
        let input = generate_text(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("ascii", size), &input, |b, input| {
            b.iter(|| estimate_tokens(black_box(input)));
        });
    }

    group.finish();
}

fn token_estimation_unicode(c: &mut Criterion) {
    let mut group = c.benchmark_group("estimate_tokens_unicode");

    let pattern = "Привет мир! 你好世界! こんにちは世界! 🌍🌎🌏 ";
    for size in [1_000, 10_000, 100_000] {
        let input = pattern.repeat(size / pattern.len() + 1);
        let input = &input[..input.floor_char_boundary(size)];
        let input = input.to_string();
        let actual_len = input.len();
        group.throughput(Throughput::Bytes(actual_len as u64));
        group.bench_with_input(
            BenchmarkId::new("unicode", actual_len),
            &input,
            |b, input| {
                b.iter(|| estimate_tokens(black_box(input)));
            },
        );
    }

    group.finish();
}

fn token_estimation_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("estimate_tokens_batch");

    let messages: Vec<String> = (0..50)
        .map(|i| format!("Message {i}: {}", generate_text(200)))
        .collect();

    group.bench_function("50_messages_sum", |b| {
        b.iter(|| black_box(messages.iter().map(|m| estimate_tokens(m)).sum::<usize>()));
    });

    group.finish();
}

criterion_group!(
    benches,
    token_estimation,
    token_estimation_unicode,
    token_estimation_batch
);
criterion_main!(benches);
