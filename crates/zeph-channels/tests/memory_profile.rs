// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Manual memory profiling test to analyze allocation patterns
/// Run with: `cargo test --test memory_profile -- --nocapture`
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
        let output_len = u32::try_from(output.len()).expect("test output length must fit in u32");
        let input_len = u32::try_from(input.len()).expect("test input length must fit in u32");
        let overhead = (f64::from(output_len) / f64::from(input_len) - 1.0) * 100.0;
        println!(
            "{}: input={} bytes, output={} bytes, overhead={:.1}%",
            name,
            input.len(),
            output.len(),
            overhead
        );
    }
}
