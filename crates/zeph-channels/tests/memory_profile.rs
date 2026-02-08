/// Manual memory profiling test to analyze allocation patterns
/// Run with: cargo test --test memory_profile -- --nocapture
use zeph_channels::markdown::markdown_to_telegram;

#[test]
fn measure_output_size_overhead() {
    // Measure escaping overhead
    let cases = vec![
        ("Plain text", "Plain text without special chars.".repeat(10)),
        ("Special chars", "Text with special: . ! - + = |".repeat(10)),
        ("Bold/italic", "**bold** and *italic* text ".repeat(10)),
        ("Code block", "```\ncode\n```\n".repeat(10)),
    ];

    for (name, input) in cases {
        let output = markdown_to_telegram(&input);
        let overhead = (output.len() as f64 / input.len() as f64 - 1.0) * 100.0;
        println!(
            "{}: input={} bytes, output={} bytes, overhead={:.1}%",
            name,
            input.len(),
            output.len(),
            overhead
        );
    }
}
