use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zeph_channels::markdown::{markdown_to_telegram, utf8_chunks};

fn generate_text(size: usize, pattern: &str) -> String {
    pattern.repeat(size / pattern.len() + 1)[..size].to_string()
}

fn generate_markdown(size: usize) -> String {
    let paragraph = "This is a **bold** paragraph with *italic* text and `inline code`. \
                     It has some special chars: - + = | { } . ! that need escaping.\n\n";
    paragraph.repeat(size / paragraph.len() + 1)[..size].to_string()
}

fn markdown_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("markdown_to_telegram");

    // Test various input sizes (typical message sizes)
    for size in [100, 500, 1000, 5000, 10000].iter() {
        let input = generate_markdown(*size);
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("typical", size), &input, |b, input| {
            b.iter(|| markdown_to_telegram(black_box(input)));
        });
    }

    group.finish();
}

fn escaping_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("heavy_escaping");

    // Text with many special characters that need escaping
    let special_chars = "Text with special: . ! - + = | { } [ ] ( ) ~ ` > # * _ \\ ";

    for size in [100, 1000, 5000].iter() {
        let input = special_chars.repeat(*size / special_chars.len() + 1)[..*size].to_string();
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(
            BenchmarkId::new("special_chars", size),
            &input,
            |b, input| {
                b.iter(|| markdown_to_telegram(black_box(input)));
            },
        );
    }

    group.finish();
}

fn code_blocks(c: &mut Criterion) {
    let mut group = c.benchmark_group("code_blocks");

    // Large code blocks with minimal escaping
    let code_pattern = "```rust\nfn main() {\n    println!(\"Hello, world!\");\n}\n```\n\n";

    for size in [500, 2000, 5000].iter() {
        let input = code_pattern.repeat(*size / code_pattern.len() + 1)[..*size].to_string();
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("code", size), &input, |b, input| {
            b.iter(|| markdown_to_telegram(black_box(input)));
        });
    }

    group.finish();
}

fn unicode_text(c: &mut Criterion) {
    let mut group = c.benchmark_group("unicode");

    // Multi-byte UTF-8 characters (emoji, CJK)
    let emoji_pattern = "🎉 Celebration 🎊 Party 🎈 Balloon 🎁 Gift ";
    let cjk_pattern = "这是中文文本。日本語のテキスト。한국어 텍스트. ";

    for (name, pattern) in [("emoji", emoji_pattern), ("cjk", cjk_pattern)] {
        let input = pattern.repeat(200);
        let size = input.len();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new(name, size), &input, |b, input| {
            b.iter(|| markdown_to_telegram(black_box(input)));
        });
    }

    group.finish();
}

fn mixed_formatting(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_formatting");

    // Complex mixed formatting
    let mixed = "# Header\n\n\
                 **Bold** and *italic* and ~~strikethrough~~.\n\n\
                 `inline code` and [link](https://example.com).\n\n\
                 > Blockquote with **bold**\n\n\
                 - List item 1\n\
                 - List item 2 with *italic*\n\n\
                 ```\ncode block\nwith multiple lines\n```\n\n";

    for size in [500, 2000, 5000].iter() {
        let input = mixed.repeat(*size / mixed.len() + 1)[..*size].to_string();
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("mixed", size), &input, |b, input| {
            b.iter(|| markdown_to_telegram(black_box(input)));
        });
    }

    group.finish();
}

fn utf8_chunking(c: &mut Criterion) {
    let mut group = c.benchmark_group("utf8_chunks");

    // Test chunking performance with different text types
    let ascii = generate_text(10000, "Plain ASCII text with newlines.\n");
    let emoji = "🎉🎊🎈🎁".repeat(500); // Multi-byte characters
    let mixed = "Text with emoji 🎉 and CJK 中文.\n".repeat(200);

    for (name, text) in [("ascii", &ascii), ("emoji", &emoji), ("mixed", &mixed)] {
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::new(name, text.len()), text, |b, text| {
            b.iter(|| utf8_chunks(black_box(text), black_box(4096)));
        });
    }

    group.finish();
}

fn plain_text_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline_plain_text");

    // Plain text without special formatting (minimal processing)
    for size in [100, 1000, 5000].iter() {
        let input = "Plain text without any special formatting or characters that need escaping besides spaces and letters"
            .repeat(*size / 100 + 1)[..*size].to_string();
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("plain", size), &input, |b, input| {
            b.iter(|| markdown_to_telegram(black_box(input)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    markdown_conversion,
    escaping_heavy,
    code_blocks,
    unicode_text,
    mixed_formatting,
    utf8_chunking,
    plain_text_baseline,
);
criterion_main!(benches);
