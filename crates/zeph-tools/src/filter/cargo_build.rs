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
        // Skip subcommands already handled by dedicated filters
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
];

fn is_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p))
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
        if exit_code != 0 {
            return make_result(raw_output, raw_output.to_owned(), FilterConfidence::Fallback);
        }

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

        if noise_count == 0 {
            return make_result(raw_output, raw_output.to_owned(), FilterConfidence::Fallback);
        }

        let mut output = String::new();
        if let Some(fin) = finished_line {
            let _ = writeln!(output, "{fin}");
        }
        if noise_count > 0 {
            let _ = writeln!(output, "({noise_count} compile/fetch lines removed)");
        }
        if !kept.is_empty() {
            output.push('\n');
            for line in &kept {
                let _ = writeln!(output, "{line}");
            }
        }

        make_result(raw_output, output.trim_end().to_owned(), FilterConfidence::Full)
    }
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
    fn preserves_full_on_error() {
        let f = make_filter();
        let raw = "error[E0308]: mismatched types\n  --> src/main.rs:10:5";
        let result = f.filter("cargo build", raw, 1);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn passthrough_when_no_noise() {
        let f = make_filter();
        let raw = "some random output\nno cargo noise here";
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
}
