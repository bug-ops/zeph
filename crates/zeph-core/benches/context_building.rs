use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use zeph_memory::estimate_tokens;

fn generate_messages(count: usize, avg_len: usize) -> Vec<String> {
    let base = "This is a simulated message with typical content for an AI conversation. ";
    (0..count)
        .map(|i| {
            let content = base.repeat(avg_len / base.len() + 1);
            format!("[user]: message {i} {}", &content[..avg_len])
        })
        .collect()
}

fn should_compact_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("should_compact");

    for count in [20, 50, 100] {
        let messages = generate_messages(count, 200);
        group.bench_with_input(BenchmarkId::new("messages", count), &messages, |b, msgs| {
            b.iter(|| {
                let total: usize = msgs.iter().map(|m| estimate_tokens(m)).sum();
                black_box(total > 4000)
            });
        });
    }

    group.finish();
}

fn trim_budget_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("trim_budget_scan");

    for count in [20, 50, 100] {
        let messages = generate_messages(count, 200);
        let budget = 2000usize;

        group.bench_with_input(BenchmarkId::new("messages", count), &messages, |b, msgs| {
            b.iter(|| {
                let mut total = 0usize;
                let mut keep_from = msgs.len();
                for i in (0..msgs.len()).rev() {
                    let tokens = estimate_tokens(&msgs[i]);
                    if total + tokens > budget {
                        break;
                    }
                    total += tokens;
                    keep_from = i;
                }
                black_box((keep_from, total))
            });
        });
    }

    group.finish();
}

fn history_formatting(c: &mut Criterion) {
    let mut group = c.benchmark_group("history_formatting");

    for count in [10, 30, 50] {
        let messages = generate_messages(count, 200);

        group.bench_with_input(BenchmarkId::new("messages", count), &messages, |b, msgs| {
            b.iter(|| {
                let text: String = msgs
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                black_box(text)
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    should_compact_check,
    trim_budget_scan,
    history_formatting
);
criterion_main!(benches);
