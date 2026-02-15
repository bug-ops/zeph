use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use zeph_skills::matcher::cosine_similarity;

fn generate_vector(dim: usize, seed: f32) -> Vec<f32> {
    (0..dim).map(|i| ((i as f32 + seed) * 0.1).sin()).collect()
}

fn cosine_similarity_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("cosine_similarity");

    for dim in [128, 384, 768, 1536] {
        let a = generate_vector(dim, 1.0);
        let b = generate_vector(dim, 2.0);
        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |bench, _| {
            bench.iter(|| cosine_similarity(black_box(&a), black_box(&b)));
        });
    }

    group.finish();
}

fn cosine_ranking(c: &mut Criterion) {
    let mut group = c.benchmark_group("cosine_ranking");

    for count in [10, 50, 100] {
        let query = generate_vector(384, 0.0);
        let candidates: Vec<Vec<f32>> =
            (0..count).map(|i| generate_vector(384, i as f32)).collect();

        group.bench_with_input(BenchmarkId::new("candidates", count), &count, |b, _| {
            b.iter(|| {
                let mut scored: Vec<(usize, f32)> = candidates
                    .iter()
                    .enumerate()
                    .map(|(i, emb)| (i, cosine_similarity(&query, emb)))
                    .collect();
                scored.sort_unstable_by(|a, b| {
                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                });
                black_box(scored)
            });
        });
    }

    group.finish();
}

criterion_group!(benches, cosine_similarity_bench, cosine_ranking);
criterion_main!(benches);
