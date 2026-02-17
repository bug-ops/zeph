use std::fmt::Write;

use super::{FilterResult, OutputFilter, make_result};

pub struct TestOutputFilter;

impl OutputFilter for TestOutputFilter {
    fn matches(&self, command: &str) -> bool {
        let cmd = command.to_lowercase();
        // Split into tokens to match "cargo [+toolchain] test" or "cargo nextest"
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        if tokens.first() != Some(&"cargo") {
            return false;
        }
        tokens
            .iter()
            .skip(1)
            .any(|t| *t == "test" || *t == "nextest")
    }

    fn filter(&self, _command: &str, raw_output: &str, exit_code: i32) -> FilterResult {
        let mut passed = 0u64;
        let mut failed = 0u64;
        let mut ignored = 0u64;
        let mut filtered_out = 0u64;
        let mut failure_blocks: Vec<String> = Vec::new();
        let mut in_failure_block = false;
        let mut current_block = String::new();
        let mut has_summary = false;

        for line in raw_output.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("FAIL [") || trimmed.starts_with("FAIL  [") {
                failed += 1;
                continue;
            }
            if trimmed.starts_with("PASS [") || trimmed.starts_with("PASS  [") {
                passed += 1;
                continue;
            }

            // Standard cargo test failure block
            if trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----") {
                in_failure_block = true;
                current_block.clear();
                current_block.push_str(line);
                current_block.push('\n');
                continue;
            }

            if in_failure_block {
                current_block.push_str(line);
                current_block.push('\n');
                if trimmed == "failures:" || trimmed.starts_with("---- ") {
                    failure_blocks.push(current_block.clone());
                    in_failure_block = trimmed.starts_with("---- ");
                    if in_failure_block {
                        current_block.clear();
                        current_block.push_str(line);
                        current_block.push('\n');
                    }
                }
                continue;
            }

            if trimmed == "failures:" && !current_block.is_empty() {
                failure_blocks.push(current_block.clone());
                current_block.clear();
            }

            // Parse summary line
            if trimmed.starts_with("test result:") {
                has_summary = true;
                for part in trimmed.split(';') {
                    let part = part.trim();
                    if let Some(n) = extract_count(part, "passed") {
                        passed += n;
                    } else if let Some(n) = extract_count(part, "failed") {
                        failed += n;
                    } else if let Some(n) = extract_count(part, "ignored") {
                        ignored += n;
                    } else if let Some(n) = extract_count(part, "filtered out") {
                        filtered_out += n;
                    }
                }
            }

            if trimmed.contains("tests run:") {
                has_summary = true;
            }
        }

        if in_failure_block && !current_block.is_empty() {
            failure_blocks.push(current_block);
        }

        if !has_summary && passed == 0 && failed == 0 {
            return make_result(raw_output, raw_output.to_owned());
        }

        let mut output = String::new();

        if exit_code != 0 && !failure_blocks.is_empty() {
            output.push_str("FAILURES:\n\n");
            for block in &failure_blocks {
                output.push_str(block);
                output.push('\n');
            }
        }

        let status = if failed > 0 { "FAILED" } else { "ok" };
        let _ = write!(
            output,
            "test result: {status}. {passed} passed; {failed} failed; \
             {ignored} ignored; {filtered_out} filtered out"
        );

        make_result(raw_output, output)
    }
}

fn extract_count(s: &str, label: &str) -> Option<u64> {
    let idx = s.find(label)?;
    let before = s[..idx].trim();
    let num_str = before.rsplit_once(' ').map_or(before, |(_, n)| n);
    let num_str = num_str.trim_end_matches('.');
    let num_str = num_str.rsplit('.').next().unwrap_or(num_str).trim();
    num_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cargo_test() {
        let f = TestOutputFilter;
        assert!(f.matches("cargo test"));
        assert!(f.matches("cargo test --workspace"));
        assert!(f.matches("cargo +nightly test"));
        assert!(f.matches("cargo nextest run"));
        assert!(!f.matches("cargo build"));
        assert!(!f.matches("cargo test-helper"));
        assert!(!f.matches("cargo install cargo-nextest"));
    }

    #[test]
    fn filter_success_compresses() {
        let f = TestOutputFilter;
        let raw = "\
running 3 tests
test foo::test_a ... ok
test foo::test_b ... ok
test foo::test_c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
";
        let result = f.filter("cargo test", raw, 0);
        assert!(result.output.contains("3 passed"));
        assert!(result.output.contains("0 failed"));
        assert!(!result.output.contains("test_a"));
        assert!(result.savings_pct() > 30.0);
    }

    #[test]
    fn filter_failure_preserves_details() {
        let f = TestOutputFilter;
        let raw = "\
running 2 tests
test foo::test_a ... ok
test foo::test_b ... FAILED

---- foo::test_b stdout ----
thread 'foo::test_b' panicked at 'assertion failed: false'

failures:
    foo::test_b

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 filtered out; finished in 0.01s
";
        let result = f.filter("cargo test", raw, 1);
        assert!(result.output.contains("FAILURES:"));
        assert!(result.output.contains("foo::test_b"));
        assert!(result.output.contains("assertion failed"));
        assert!(result.output.contains("1 failed"));
    }

    #[test]
    fn filter_no_summary_passthrough() {
        let f = TestOutputFilter;
        let raw = "some random output with no test results";
        let result = f.filter("cargo test", raw, 0);
        assert_eq!(result.output, raw);
    }
}
