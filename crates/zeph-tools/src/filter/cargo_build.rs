use std::fmt::Write;
use std::sync::LazyLock;

use super::{
    CargoBuildFilterConfig, CommandMatcher, FilterConfidence, FilterResult, OutputFilter,
    make_result,
};

static CARGO_BUILD_MATCHER: LazyLock<CommandMatcher> = LazyLock::new(|| {
    CommandMatcher::Custom(Box::new(|cmd| {
        let c = cmd.to_lowercase();
        let tokens: Vec<&str> = c.split_whitespace().collect();
        if tokens.first() != Some(&"cargo") {
            return false;
        }
        let dominated = ["test", "nextest", "clippy"];
        !tokens.iter().skip(1).any(|t| dominated.contains(t))
    }))
});

const NOISE_PREFIXES: &[&str] = &[
    "Compiling ",
    "Downloading ",
    "Downloaded ",
    "Updating ",
    "Fetching ",
    "Fresh ",
    "Packaging ",
    "Verifying ",
    "Archiving ",
    "Locking ",
    "Adding ",
    "Removing ",
    "Checking ",
    "Documenting ",
    "Running ",
    "Loaded ",
    "Blocking ",
    "Unpacking ",
];

/// Max lines to keep when output has no recognizable noise pattern.
const LONG_OUTPUT_THRESHOLD: usize = 30;
const KEEP_HEAD: usize = 10;
const KEEP_TAIL: usize = 5;

fn is_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// Check if a line is cargo build/fetch noise (for reuse by other filters).
pub fn is_cargo_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Finished ") || is_noise(line)
}

pub struct CargoBuildFilter;

impl CargoBuildFilter {
    #[must_use]
    pub fn new(_config: CargoBuildFilterConfig) -> Self {
        Self
    }
}

impl OutputFilter for CargoBuildFilter {
    fn name(&self) -> &'static str {
        "cargo_build"
    }

    fn matcher(&self) -> &CommandMatcher {
        &CARGO_BUILD_MATCHER
    }

    fn filter(&self, _command: &str, raw_output: &str, exit_code: i32) -> FilterResult {
        let mut noise_count = 0usize;
        let mut kept = Vec::new();
        let mut finished_line: Option<&str> = None;

        for line in raw_output.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Finished ") {
                finished_line = Some(trimmed);
                noise_count += 1;
            } else if is_noise(line) {
                noise_count += 1;
            } else {
                kept.push(line);
            }
        }

        if noise_count > 0 {
            return build_noise_result(raw_output, &kept, finished_line, noise_count);
        }

        if exit_code != 0 {
            return make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            );
        }

        // No recognizable noise — apply generic long-output truncation
        let lines: Vec<&str> = raw_output.lines().collect();
        if lines.len() > LONG_OUTPUT_THRESHOLD {
            return truncate_long(raw_output, &lines);
        }

        make_result(
            raw_output,
            raw_output.to_owned(),
            FilterConfidence::Fallback,
        )
    }
}

fn build_noise_result(
    raw: &str,
    kept: &[&str],
    finished_line: Option<&str>,
    noise_count: usize,
) -> FilterResult {
    let mut output = String::new();
    if let Some(fin) = finished_line {
        let _ = writeln!(output, "{fin}");
    }
    let _ = writeln!(output, "({noise_count} compile/fetch lines removed)");
    if !kept.is_empty() {
        output.push('\n');
        if kept.len() > LONG_OUTPUT_THRESHOLD {
            let omitted = kept.len() - KEEP_HEAD - KEEP_TAIL;
            for line in &kept[..KEEP_HEAD] {
                let _ = writeln!(output, "{line}");
            }
            let _ = writeln!(output, "\n... ({omitted} lines omitted) ...\n");
            for line in &kept[kept.len() - KEEP_TAIL..] {
                let _ = writeln!(output, "{line}");
            }
        } else {
            for line in kept {
                let _ = writeln!(output, "{line}");
            }
        }
    }
    make_result(raw, output.trim_end().to_owned(), FilterConfidence::Full)
}

fn truncate_long(raw: &str, lines: &[&str]) -> FilterResult {
    let total = lines.len();
    let omitted = total - KEEP_HEAD - KEEP_TAIL;
    let mut output = String::new();
    for line in &lines[..KEEP_HEAD] {
        let _ = writeln!(output, "{line}");
    }
    let _ = writeln!(output, "\n... ({omitted} lines omitted) ...\n");
    for line in &lines[total - KEEP_TAIL..] {
        let _ = writeln!(output, "{line}");
    }
    make_result(raw, output.trim_end().to_owned(), FilterConfidence::Partial)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter() -> CargoBuildFilter {
        CargoBuildFilter::new(CargoBuildFilterConfig::default())
    }

    #[test]
    fn matches_cargo_build_commands() {
        let f = make_filter();
        assert!(f.matcher().matches("cargo build"));
        assert!(f.matcher().matches("cargo build --release"));
        assert!(f.matcher().matches("cargo doc --no-deps"));
        assert!(f.matcher().matches("cargo +nightly fmt --check"));
        assert!(f.matcher().matches("cargo audit"));
        assert!(f.matcher().matches("cargo tree --duplicates"));
        assert!(f.matcher().matches("cargo bench"));
    }

    #[test]
    fn skips_test_and_clippy() {
        let f = make_filter();
        assert!(!f.matcher().matches("cargo test"));
        assert!(!f.matcher().matches("cargo nextest run"));
        assert!(!f.matcher().matches("cargo clippy --workspace"));
    }

    #[test]
    fn filters_compile_noise() {
        let f = make_filter();
        let raw = "    Compiling serde v1.0.200\n    Compiling zeph-core v0.9.9\n    Compiling zeph-tools v0.9.9\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.32s";
        let result = f.filter("cargo build", raw, 0);
        assert_eq!(result.confidence, FilterConfidence::Full);
        assert!(result.output.contains("Finished"));
        assert!(result.output.contains("4 compile/fetch lines removed"));
        assert!(!result.output.contains("Compiling"));
    }

    #[test]
    fn filters_audit_noise() {
        let f = make_filter();
        let raw = "    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`\n      Loaded 920 security advisories (from /Users/rabax/.cargo/advisory-db)\n    Updating crates.io index\n0 vulnerabilities found";
        let result = f.filter("cargo audit", raw, 1);
        assert_eq!(result.confidence, FilterConfidence::Full);
        assert!(result.output.contains("3 compile/fetch lines removed"));
        assert!(result.output.contains("0 vulnerabilities found"));
        assert!(!result.output.contains("Fetching"));
    }

    #[test]
    fn truncates_long_tree_output() {
        let f = make_filter();
        let mut lines = Vec::new();
        for i in 0..80 {
            lines.push(format!("├── dep-{i} v0.1.{i}"));
        }
        let raw = lines.join("\n");
        let result = f.filter("cargo tree", &raw, 0);
        assert_eq!(result.confidence, FilterConfidence::Partial);
        assert!(result.output.contains("lines omitted"));
        assert!(result.output.contains("dep-0"));
        assert!(result.output.contains("dep-79"));
    }

    #[test]
    fn preserves_full_on_error() {
        let f = make_filter();
        let raw = "error[E0308]: mismatched types\n  --> src/main.rs:10:5";
        let result = f.filter("cargo build", raw, 1);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn passthrough_short_output() {
        let f = make_filter();
        let raw = "some short output\nonly two lines";
        let result = f.filter("cargo build", raw, 0);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn keeps_non_noise_lines() {
        let f = make_filter();
        let raw = "    Compiling zeph-core v0.9.9\nwarning: unused import\n  --> src/lib.rs:5:1\n    Finished `dev` profile target(s) in 2.00s";
        let result = f.filter("cargo build", raw, 0);
        assert!(result.output.contains("warning: unused import"));
        assert!(result.output.contains("src/lib.rs:5:1"));
        assert!(!result.output.contains("Compiling"));
    }

    #[test]
    fn cargo_build_filter_snapshot() {
        let f = make_filter();
        let raw = "\
   Compiling zeph-core v0.11.0
   Compiling zeph-tools v0.11.0
   Compiling zeph-llm v0.11.0
warning: unused import: `std::fmt`
  --> crates/zeph-core/src/lib.rs:3:5
   |
3  |     use std::fmt;
   |         ^^^^^^^^
   = note: `#[warn(unused_imports)]` on by default
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.23s";
        let result = f.filter("cargo build", raw, 0);
        insta::assert_snapshot!(result.output);
    }

    #[test]
    fn cargo_build_error_snapshot() {
        let f = make_filter();
        let raw = "\
   Compiling zeph-core v0.11.0
error[E0308]: mismatched types
  --> crates/zeph-core/src/lib.rs:10:5
   |
10 |     return 42;
   |            ^^ expected `()`, found integer
error: could not compile `zeph-core` due to 1 previous error";
        let result = f.filter("cargo build", raw, 1);
        insta::assert_snapshot!(result.output);
    }
}
