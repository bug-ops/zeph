// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use super::*;

fn strip_noise_filter(patterns: &[&str]) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-strip"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::StripNoise {
            patterns: patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
        },
    }
}

fn truncate_filter(max_lines: usize, head: usize, tail: usize) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-truncate"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::Truncate {
            max_lines,
            head,
            tail,
        },
    }
}

fn keep_matching_filter(patterns: &[&str]) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-keep"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::KeepMatching {
            patterns: patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
        },
    }
}

fn strip_annotated_filter(patterns: &[&str], summary_pattern: Option<&str>) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-annotated"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::StripAnnotated {
            patterns: patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
            summary_pattern: summary_pattern.map(|p| Regex::new(p).unwrap()),
            long_output_threshold: 30,
            keep_head: 10,
            keep_tail: 5,
        },
    }
}

fn test_summary_filter() -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-summary"),
        matcher: CommandMatcher::Prefix(Arc::from("cargo test")),
        strategy: CompiledStrategy::TestSummary {
            max_failures: 10,
            truncate_stack_trace: 50,
        },
    }
}

fn group_by_rule_filter(location_pattern: &str, rule_pattern: &str) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-group"),
        matcher: CommandMatcher::Prefix(Arc::from("cargo clippy")),
        strategy: CompiledStrategy::GroupByRule {
            location_re: Regex::new(location_pattern).unwrap(),
            rule_re: Regex::new(rule_pattern).unwrap(),
        },
    }
}

fn git_status_filter() -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-git-status"),
        matcher: CommandMatcher::Prefix(Arc::from("git status")),
        strategy: CompiledStrategy::GitStatus,
    }
}

fn git_diff_filter(max_diff_lines: usize) -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-git-diff"),
        matcher: CommandMatcher::Prefix(Arc::from("git diff")),
        strategy: CompiledStrategy::GitDiff { max_diff_lines },
    }
}

fn dedup_filter() -> DeclarativeFilter {
    DeclarativeFilter {
        name: Arc::from("test-dedup"),
        matcher: CommandMatcher::Prefix(Arc::from("journalctl")),
        strategy: CompiledStrategy::Dedup {
            normalize_patterns: vec![
                (
                    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.\d]*)?([Z+-][\d:]*)?")
                        .unwrap(),
                    "<TS>".into(),
                ),
                (
                    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
                        .unwrap(),
                    "<UUID>".into(),
                ),
                (
                    Regex::new(r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}").unwrap(),
                    "<IP>".into(),
                ),
                (
                    Regex::new(r"(?:port|pid|PID)[=: ]+\d+").unwrap(),
                    "<N>".into(),
                ),
            ],
            max_unique_patterns: 10_000,
        },
    }
}

// --- compile_match ---

#[test]
fn compile_match_exact() {
    let m = MatchConfig {
        exact: Some("ls".into()),
        prefix: None,
        regex: None,
    };
    let matcher = compile_match(&m).unwrap();
    assert!(matches!(matcher, CommandMatcher::Exact(_)));
}

#[test]
fn compile_match_prefix() {
    let m = MatchConfig {
        exact: None,
        prefix: Some("docker ".into()),
        regex: None,
    };
    let matcher = compile_match(&m).unwrap();
    assert!(matches!(matcher, CommandMatcher::Prefix(_)));
    assert!(matcher.matches("docker build ."));
}

#[test]
fn compile_match_regex() {
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: Some(r"^npm\s+install".into()),
    };
    let matcher = compile_match(&m).unwrap();
    assert!(matcher.matches("npm install"));
    assert!(!matcher.matches("yarn install"));
}

#[test]
fn compile_match_invalid_regex_returns_error() {
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: Some("[invalid".into()),
    };
    assert!(compile_match(&m).is_err());
}

#[test]
fn compile_match_empty_returns_error() {
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: None,
    };
    assert!(compile_match(&m).is_err());
}

// --- compile_strategy ---

#[test]
fn compile_strategy_strip_noise_valid() {
    let s = StrategyConfig::StripNoise {
        patterns: vec![r"^\s*$".into(), r"^noise".into()],
    };
    let compiled = compile_strategy(s).unwrap();
    assert!(matches!(compiled, CompiledStrategy::StripNoise { .. }));
}

#[test]
fn compile_strategy_strip_noise_invalid_pattern() {
    let s = StrategyConfig::StripNoise {
        patterns: vec!["[broken".into()],
    };
    assert!(compile_strategy(s).is_err());
}

#[test]
fn compile_strategy_truncate_valid() {
    let s = StrategyConfig::Truncate {
        max_lines: 50,
        head: 10,
        tail: 10,
    };
    let compiled = compile_strategy(s).unwrap();
    assert!(matches!(
        compiled,
        CompiledStrategy::Truncate {
            max_lines: 50,
            head: 10,
            tail: 10
        }
    ));
}

#[test]
fn compile_strategy_truncate_head_tail_exceeds_max() {
    let s = StrategyConfig::Truncate {
        max_lines: 10,
        head: 8,
        tail: 5,
    };
    assert!(compile_strategy(s).is_err());
}

#[test]
fn compile_strategy_keep_matching_valid() {
    let s = StrategyConfig::KeepMatching {
        patterns: vec!["->".into(), r"^To ".into()],
    };
    assert!(compile_strategy(s).is_ok());
}

#[test]
fn compile_strategy_group_by_rule_invalid_regex() {
    let s = StrategyConfig::GroupByRule {
        location_pattern: "[broken".into(),
        rule_pattern: r"#\[warn\(([^)]+)\)\]".into(),
    };
    assert!(compile_strategy(s).is_err());
}

// --- DeclarativeFilter::filter (strip_noise) ---

#[test]
fn strip_noise_removes_matching_lines() {
    let f = strip_noise_filter(&[r"^noise:", r"^\s*$"]);
    let raw = "noise: ignore this\nkeep this\nnoise: also ignore\nkeep too";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("keep this"));
    assert!(result.output.contains("keep too"));
    assert!(!result.output.contains("noise:"));
}

#[test]
fn strip_noise_returns_fallback_when_nothing_removed() {
    let f = strip_noise_filter(&[r"^NOMATCH"]);
    let raw = "line one\nline two";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
    assert!(result.output.contains("line one"));
}

#[test]
fn strip_noise_strips_ansi_before_matching() {
    let f = strip_noise_filter(&[r"^noise"]);
    let raw = "\x1b[32mnoise\x1b[0m: colored noise\nclean line";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(!result.output.contains("noise"));
    assert!(result.output.contains("clean line"));
}

// --- DeclarativeFilter::filter (truncate) ---

#[test]
fn truncate_short_output_passthrough() {
    let f = truncate_filter(50, 10, 10);
    let raw = "line1\nline2\nline3";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
    assert!(result.output.contains("line1"));
    assert!(result.output.contains("line3"));
}

#[test]
fn truncate_long_output_applies_head_tail() {
    let f = truncate_filter(10, 3, 3);
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    let raw = lines.join("\n");
    let result = f.filter("cmd", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("line 0"));
    assert!(result.output.contains("line 1"));
    assert!(result.output.contains("line 2"));
    assert!(result.output.contains("line 17"));
    assert!(result.output.contains("line 18"));
    assert!(result.output.contains("line 19"));
    assert!(result.output.contains("lines omitted"));
    assert!(!result.output.contains("line 3"));
}

#[test]
fn truncate_omitted_count_correct() {
    let f = truncate_filter(10, 2, 2);
    let lines: Vec<String> = (0..20).map(|i| format!("L{i}")).collect();
    let raw = lines.join("\n");
    let result = f.filter("cmd", &raw, 0);
    assert!(result.output.contains("16 lines omitted"));
}

// --- keep_matching ---

#[test]
fn keep_matching_keeps_only_matching_lines() {
    let f = keep_matching_filter(&["->", r"^To "]);
    let raw = "\
Enumerating objects: 5, done.
To github.com:user/repo.git
   abc1234..def5678  main -> main
";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("->"));
    assert!(result.output.contains("To github.com"));
    assert!(!result.output.contains("Enumerating"));
}

#[test]
fn keep_matching_fallback_when_nothing_matches() {
    let f = keep_matching_filter(&[r"^NOMATCH"]);
    let raw = "some output\nno matches here";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- strip_annotated ---

#[test]
fn strip_annotated_removes_noise_with_count() {
    let f = strip_annotated_filter(
        &[r"^\s*Compiling ", r"^\s*Checking "],
        Some(r"^\s*Finished "),
    );
    let raw = "    Compiling serde v1.0\n    Checking foo\n    Finished dev in 1s\nerror: oops";
    let result = f.filter("cargo build", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("noise lines removed"));
    assert!(result.output.contains("Finished"));
    assert!(!result.output.contains("Compiling"));
}

#[test]
fn strip_annotated_passthrough_on_error_no_noise() {
    let f = strip_annotated_filter(&[r"^\s*Compiling "], None);
    let raw = "error[E0308]: mismatched types\n  --> src/main.rs:10:5";
    let result = f.filter("cargo build", raw, 1);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
    assert_eq!(result.output, raw);
}

#[test]
fn strip_annotated_passthrough_short_no_noise() {
    let f = strip_annotated_filter(&[r"^\s*Compiling "], None);
    let raw = "short output\nno noise";
    let result = f.filter("cargo build", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- test_summary ---

#[test]
fn test_summary_success_compresses() {
    let f = test_summary_filter();
    let raw = "\
running 3 tests
test foo::test_a ... ok
test foo::test_b ... ok
test foo::test_c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
";
    let result = f.filter("cargo test", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("3 passed"));
    assert!(result.output.contains("0 failed"));
    assert!(!result.output.contains("test_a"));
    assert!(result.savings_pct() > 30.0);
}

#[test]
fn test_summary_failure_preserves_details() {
    let f = test_summary_filter();
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
    assert!(result.output.contains("assertion failed"));
    assert!(result.output.contains("1 failed"));
}

#[test]
fn test_summary_no_summary_passthrough() {
    let f = test_summary_filter();
    let raw = "some random output with no test results";
    let result = f.filter("cargo test", raw, 0);
    assert_eq!(result.output, raw);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- group_by_rule (clippy) ---

#[test]
fn group_by_rule_groups_warnings() {
    let f = group_by_rule_filter(r"^\s*-->\s*(.+:\d+)", r"#\[warn\(([^)]+)\)\]");
    let raw = "\
warning: needless pass by value
  --> src/foo.rs:12:5
   |
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: needless pass by value
  --> src/bar.rs:45:10
   |
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: unused import
  --> src/main.rs:5:1
   |
   = note: `#[warn(clippy::unused_imports)]` on by default
";
    let result = f.filter("cargo clippy", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(
        result
            .output
            .contains("clippy::needless_pass_by_value (2 warnings):")
    );
    assert!(result.output.contains("src/foo.rs:12"));
    assert!(
        result
            .output
            .contains("clippy::unused_imports (1 warning):")
    );
    assert!(result.output.contains("3 warnings total (2 rules)"));
}

#[test]
fn group_by_rule_error_passthrough() {
    let f = group_by_rule_filter(r"^\s*-->\s*(.+:\d+)", r"#\[warn\(([^)]+)\)\]");
    let raw = "error[E0308]: mismatched types\n  --> src/main.rs:10:5\nfull details here";
    let result = f.filter("cargo clippy", raw, 1);
    assert_eq!(result.output, raw);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn group_by_rule_no_warnings_strips_cargo_noise() {
    let f = group_by_rule_filter(r"^\s*-->\s*(.+:\d+)", r"#\[warn\(([^)]+)\)\]");
    let raw = "Checking my-crate v0.1.0\n    Finished dev [unoptimized] target(s)";
    let result = f.filter("cargo clippy", raw, 0);
    assert!(result.output.is_empty());
    assert_eq!(result.confidence, FilterConfidence::Partial);
}

// --- git_status ---

#[test]
fn git_status_summarizes_short_format() {
    let f = git_status_filter();
    let raw = " M src/main.rs\n M src/lib.rs\n?? new_file.txt\nA  added.rs\n";
    let result = f.filter("git status --short", raw, 0);
    assert!(result.output.contains("M  2 files"));
    assert!(result.output.contains("??  1 files"));
    assert!(result.output.contains("A  1 files"));
    assert_eq!(result.confidence, FilterConfidence::Full);
}

#[test]
fn git_status_summarizes_long_format() {
    let f = git_status_filter();
    let raw = "\
On branch main
Changes not staged for commit:
    modified:   src/main.rs
    modified:   src/lib.rs
    deleted:    old_file.rs

Untracked files:
    new_file.txt
";
    let result = f.filter("git status", raw, 0);
    assert!(result.output.contains("M  2 files"));
    assert!(result.output.contains("D  1 files"));
}

#[test]
fn git_status_empty_fallback() {
    let f = git_status_filter();
    let raw = "nothing to commit, working tree clean";
    let result = f.filter("git status", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- git_diff ---

#[test]
fn git_diff_compresses() {
    let f = git_diff_filter(500);
    let raw = "\
diff --git a/src/main.rs b/src/main.rs
index abc..def 100644
--- a/src/main.rs
+++ b/src/main.rs
+new line 1
+new line 2
-old line 1
diff --git a/src/lib.rs b/src/lib.rs
index ghi..jkl 100644
--- a/src/lib.rs
+++ b/src/lib.rs
+added
";
    let result = f.filter("git diff", raw, 0);
    assert!(result.output.contains("src/main.rs"));
    assert!(result.output.contains("src/lib.rs"));
    assert!(result.output.contains("2 files changed"));
    assert!(result.output.contains("3 insertions(+)"));
    assert!(result.output.contains("1 deletions(-)"));
    assert_eq!(result.confidence, FilterConfidence::Full);
}

#[test]
fn git_diff_empty_fallback() {
    let f = git_diff_filter(500);
    let result = f.filter("git diff", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn git_diff_truncation_note() {
    let f = git_diff_filter(5);
    let mut raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n".to_owned();
    for i in 0..10 {
        let _ = writeln!(raw, "+line {i}");
    }
    let result = f.filter("git diff", &raw, 0);
    assert!(result.output.contains("truncated from"));
}

// --- dedup ---

#[test]
fn dedup_deduplicates_log_lines() {
    let f = dedup_filter();
    let raw = "\
2024-01-15T12:00:01Z INFO request handled path=/api/health
2024-01-15T12:00:02Z INFO request handled path=/api/health
2024-01-15T12:00:03Z INFO request handled path=/api/health
2024-01-15T12:00:04Z WARN connection timeout addr=10.0.0.1
2024-01-15T12:00:05Z WARN connection timeout addr=10.0.0.2
2024-01-15T12:00:06Z ERROR database unreachable
";
    let result = f.filter("journalctl -u app", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("(x3)"));
    assert!(result.output.contains("(x2)"));
    assert!(result.output.contains("3 unique patterns (6 total lines)"));
    assert!(result.savings_pct() > 20.0);
}

#[test]
fn dedup_all_unique_fallback() {
    let f = dedup_filter();
    let raw = "line one\nline two\nline three";
    let result = f.filter("cat app.log", raw, 0);
    assert_eq!(result.output, raw);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn dedup_short_fallback() {
    let f = dedup_filter();
    let raw = "single line";
    let result = f.filter("cat app.log", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn dedup_normalize_replaces_patterns() {
    let patterns = vec![
        (
            Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.\d]*)?([Z+-][\d:]*)?").unwrap(),
            "<TS>".into(),
        ),
        (
            Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap(),
            "<UUID>".into(),
        ),
        (
            Regex::new(r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}").unwrap(),
            "<IP>".into(),
        ),
        (
            Regex::new(r"(?:port|pid|PID)[=: ]+\d+").unwrap(),
            "<N>".into(),
        ),
    ];
    let line =
        "2024-01-15T12:00:00Z req=abc12345-1234-1234-1234-123456789012 addr=192.168.1.1 pid=1234";
    let n = dedup_normalize(line, &patterns);
    assert!(n.contains("<TS>"));
    assert!(n.contains("<UUID>"));
    assert!(n.contains("<IP>"));
    assert!(n.contains("<N>"));
}

// --- is_cargo_noise ---

#[test]
fn is_cargo_noise_detects_prefixes() {
    assert!(is_cargo_noise("   Compiling foo v1.0"));
    assert!(is_cargo_noise("   Finished dev profile"));
    assert!(is_cargo_noise("   Checking foo v1.0"));
    assert!(!is_cargo_noise("error[E0308]: mismatched types"));
    assert!(!is_cargo_noise("warning: unused import"));
}

// --- load_declarative_filters ---

#[test]
fn embedded_defaults_parse_without_error() {
    let filters = load_declarative_filters(None);
    assert!(
        !filters.is_empty(),
        "embedded defaults should produce at least one filter"
    );
}

#[test]
fn load_declarative_filters_from_missing_dir_uses_defaults() {
    let tmp = std::path::Path::new("/tmp/zeph-test-nonexistent-99999");
    let filters = load_declarative_filters(Some(tmp));
    assert!(!filters.is_empty());
}

#[test]
fn load_declarative_filters_from_custom_file() {
    let toml = r#"
[[rules]]
name = "custom-test"
match = { prefix = "myapp" }
strategy = { type = "strip_noise", patterns = ["^DEBUG"] }
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("filters.toml"), toml).unwrap();
    let filters = load_declarative_filters(Some(dir.path()));
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].name(), "custom-test");
}

#[test]
fn load_declarative_filters_skips_disabled_rules() {
    let toml = r#"
[[rules]]
name = "enabled-rule"
match = { prefix = "cmd1" }
strategy = { type = "strip_noise", patterns = ["^noise"] }
enabled = true

[[rules]]
name = "disabled-rule"
match = { prefix = "cmd2" }
strategy = { type = "strip_noise", patterns = ["^noise"] }
enabled = false
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("filters.toml"), toml).unwrap();
    let filters = load_declarative_filters(Some(dir.path()));
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].name(), "enabled-rule");
}

#[test]
fn compile_match_regex_over_512_chars_rejected() {
    let long_pattern = "a".repeat(513);
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: Some(long_pattern),
    };
    let err = compile_match(&m).unwrap_err();
    assert!(err.contains("512"), "error should mention limit: {err}");
}

#[test]
fn compile_match_regex_exactly_512_chars_accepted() {
    let pattern = "a".repeat(512);
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: Some(pattern),
    };
    assert!(compile_match(&m).is_ok());
}

#[test]
fn compile_strategy_strip_noise_pattern_over_512_chars_rejected() {
    let long_pattern = "b".repeat(513);
    let s = StrategyConfig::StripNoise {
        patterns: vec![long_pattern],
    };
    match compile_strategy(s) {
        Err(e) => assert!(e.contains("512"), "error should mention limit: {e}"),
        Ok(_) => panic!("expected error for oversized pattern"),
    }
}

#[test]
fn load_declarative_filters_oversized_file_uses_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filters.toml");
    let chunk = "# filler\n".repeat(120_000);
    std::fs::write(&path, chunk).unwrap();
    let filters = load_declarative_filters(Some(dir.path()));
    assert!(!filters.is_empty(), "should fall back to embedded defaults");
}

#[test]
fn load_declarative_filters_invalid_toml_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("filters.toml"), "[[invalid toml {{{").unwrap();
    let filters = load_declarative_filters(Some(dir.path()));
    assert!(filters.is_empty());
}

#[test]
fn load_declarative_filters_skips_invalid_regex() {
    let toml = r#"
[[rules]]
name = "bad-rule"
match = { prefix = "cmd" }
strategy = { type = "strip_noise", patterns = ["[broken"] }

[[rules]]
name = "good-rule"
match = { prefix = "cmd" }
strategy = { type = "strip_noise", patterns = ["^noise"] }
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("filters.toml"), toml).unwrap();
    let filters = load_declarative_filters(Some(dir.path()));
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].name(), "good-rule");
}

// --- TOML parsing round-trips ---

#[test]
fn toml_parse_strip_noise_rule() {
    let toml = r#"
[[rules]]
name = "docker-build"
match = { prefix = "docker build" }
strategy = { type = "strip_noise", patterns = ["^Step \\d+", "^\\s*$"] }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    assert_eq!(f.rules.len(), 1);
    assert_eq!(f.rules[0].name, "docker-build");
    assert!(f.rules[0].enabled);
    assert!(matches!(
        f.rules[0].strategy,
        StrategyConfig::StripNoise { .. }
    ));
}

#[test]
fn toml_parse_truncate_rule() {
    let toml = r#"
[[rules]]
name = "make"
match = { prefix = "make" }
strategy = { type = "truncate", max_lines = 80, head = 15, tail = 15 }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    assert_eq!(f.rules.len(), 1);
    if let StrategyConfig::Truncate {
        max_lines,
        head,
        tail,
    } = f.rules[0].strategy
    {
        assert_eq!(max_lines, 80);
        assert_eq!(head, 15);
        assert_eq!(tail, 15);
    } else {
        panic!("expected truncate strategy");
    }
}

#[test]
fn toml_parse_truncate_default_head_tail() {
    let toml = r#"
[[rules]]
name = "big-output"
match = { exact = "big" }
strategy = { type = "truncate", max_lines = 100 }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    if let StrategyConfig::Truncate { head, tail, .. } = f.rules[0].strategy {
        assert_eq!(head, 20);
        assert_eq!(tail, 20);
    } else {
        panic!("expected truncate strategy");
    }
}

#[test]
fn toml_parse_test_summary_rule() {
    let toml = r#"
[[rules]]
name = "cargo-test"
match = { regex = "^cargo\\s+test" }
strategy = { type = "test_summary", max_failures = 5, truncate_stack_trace = 30 }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    if let StrategyConfig::TestSummary {
        max_failures,
        truncate_stack_trace,
    } = f.rules[0].strategy
    {
        assert_eq!(max_failures, 5);
        assert_eq!(truncate_stack_trace, 30);
    } else {
        panic!("expected test_summary strategy");
    }
}

#[test]
fn toml_parse_git_status_rule() {
    let toml = r#"
[[rules]]
name = "git-status"
match = { regex = "^git\\s+status" }
strategy = { type = "git_status" }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    assert!(matches!(f.rules[0].strategy, StrategyConfig::GitStatus {}));
}

#[test]
fn toml_parse_dedup_default_patterns() {
    let toml = r#"
[[rules]]
name = "log-dedup"
match = { regex = "journalctl" }
strategy = { type = "dedup" }
"#;
    let f: DeclarativeFilterFile = toml::from_str(toml).unwrap();
    if let StrategyConfig::Dedup {
        normalize_patterns,
        max_unique_patterns,
    } = &f.rules[0].strategy
    {
        assert_eq!(normalize_patterns.len(), 4);
        assert_eq!(*max_unique_patterns, 10_000);
    } else {
        panic!("expected dedup strategy");
    }
}

#[test]
fn toml_parse_empty_rules() {
    let f: DeclarativeFilterFile = toml::from_str("").unwrap();
    assert!(f.rules.is_empty());
}

// --- Integration: register in registry and apply ---

#[test]
fn registry_applies_declarative_filter() {
    use super::super::{FilterConfig, OutputFilterRegistry};

    let toml = r#"
[[rules]]
name = "custom-npm"
match = { prefix = "npm install" }
strategy = { type = "strip_noise", patterns = ["^npm warn", "^npm notice"] }
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("filters.toml"), toml).unwrap();

    let config = FilterConfig {
        filters_path: Some(dir.path().to_path_buf()),
        ..FilterConfig::default()
    };

    let registry = OutputFilterRegistry::default_filters(&config);
    let raw = "npm warn deprecated pkg\nnpm notice created tarball\nDone installing";
    let result = registry.apply("npm install lodash", raw, 0);
    assert!(result.is_some());
    let out = result.unwrap();
    assert!(!out.output.contains("npm warn"));
    assert!(!out.output.contains("npm notice"));
    assert!(out.output.contains("Done installing"));
}

// --- REQ-1: HashMap::with_capacity for dedup ---

#[test]
fn dedup_cap_respected_does_not_panic_with_large_max_unique() {
    // Validates that HashMap::with_capacity(max_unique.min(4096)) doesn't OOM
    let f = DeclarativeFilter {
        name: Arc::from("test-dedup-cap"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::Dedup {
            normalize_patterns: vec![],
            max_unique_patterns: usize::MAX,
        },
    };
    let raw = "line a\nline b\nline c\nline d";
    let result = f.filter("cmd", raw, 0);
    // All unique → fallback
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- REQ-2: reject unescaped $ in Dedup replacement ---

#[test]
fn compile_dedup_rejects_dollar_replacement() {
    let s = StrategyConfig::Dedup {
        normalize_patterns: vec![NormalizeEntry {
            pattern: r"\d+".into(),
            replacement: "$1".into(),
        }],
        max_unique_patterns: 100,
    };
    match compile_strategy(s) {
        Err(e) => assert!(e.contains("unescaped '$'"), "got: {e}"),
        Ok(_) => panic!("expected error for unescaped '$' in replacement"),
    }
}

#[test]
fn compile_dedup_rejects_dollar_brace_replacement() {
    let s = StrategyConfig::Dedup {
        normalize_patterns: vec![NormalizeEntry {
            pattern: r"\w+".into(),
            replacement: "${name}".into(),
        }],
        max_unique_patterns: 100,
    };
    assert!(compile_strategy(s).is_err());
}

#[test]
fn compile_dedup_accepts_plain_text_replacement() {
    let s = StrategyConfig::Dedup {
        normalize_patterns: vec![NormalizeEntry {
            pattern: r"\d{4}-\d{2}-\d{2}".into(),
            replacement: "<TS>".into(),
        }],
        max_unique_patterns: 100,
    };
    assert!(compile_strategy(s).is_ok());
}

// --- REQ-3: empty patterns rejected for strip_noise, keep_matching, strip_annotated ---

#[test]
fn compile_strip_noise_empty_patterns_rejected() {
    let s = StrategyConfig::StripNoise { patterns: vec![] };
    assert!(compile_strategy(s).is_err());
}

#[test]
fn compile_keep_matching_empty_patterns_rejected() {
    let s = StrategyConfig::KeepMatching { patterns: vec![] };
    assert!(compile_strategy(s).is_err());
}

#[test]
fn compile_strip_annotated_empty_patterns_rejected() {
    let s = StrategyConfig::StripAnnotated {
        patterns: vec![],
        summary_pattern: None,
        long_output_threshold: 30,
        keep_head: 10,
        keep_tail: 5,
    };
    assert!(compile_strategy(s).is_err());
}

// --- ADV-2: no panic when head+tail > remaining non-noise lines ---

#[test]
fn strip_annotated_no_panic_when_head_tail_exceeds_kept() {
    let f = DeclarativeFilter {
        name: Arc::from("test-adv2"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::StripAnnotated {
            patterns: vec![Regex::new(r"^NOISE").unwrap()],
            summary_pattern: None,
            long_output_threshold: 2,
            keep_head: 10,
            keep_tail: 5,
        },
    };
    let mut raw = String::new();
    for i in 0..10 {
        let _ = writeln!(raw, "NOISE line {i}");
    }
    raw.push_str("kept 1\nkept 2\nkept 3\n");
    let result = f.filter("cmd", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
}

#[test]
fn strip_annotated_no_panic_single_kept_line_large_head_tail() {
    let f = DeclarativeFilter {
        name: Arc::from("test-adv2-single"),
        matcher: CommandMatcher::Prefix(Arc::from("cmd")),
        strategy: CompiledStrategy::StripAnnotated {
            patterns: vec![Regex::new(r"^NOISE").unwrap()],
            summary_pattern: None,
            long_output_threshold: 0,
            keep_head: 20,
            keep_tail: 20,
        },
    };
    let raw = "NOISE a\nNOISE b\nNOISE c\nonly kept line\n";
    let result = f.filter("cmd", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("only kept line"));
}

// --- edge cases ---

#[test]
fn strip_noise_empty_input_returns_fallback() {
    let f = strip_noise_filter(&[r"^noise"]);
    let result = f.filter("cmd", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn truncate_empty_input_returns_fallback() {
    let f = truncate_filter(10, 3, 3);
    let result = f.filter("cmd", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- snapshot tests (migrated from deleted modules) ---

#[test]
fn cargo_build_filter_snapshot() {
    let f = strip_annotated_filter(
        &[
            r"^\s*Compiling ",
            r"^\s*Downloading ",
            r"^\s*Downloaded ",
            r"^\s*Updating ",
            r"^\s*Fetching ",
            r"^\s*Fresh ",
            r"^\s*Packaging ",
            r"^\s*Verifying ",
            r"^\s*Archiving ",
            r"^\s*Locking ",
            r"^\s*Adding ",
            r"^\s*Removing ",
            r"^\s*Checking ",
            r"^\s*Documenting ",
            r"^\s*Running ",
            r"^\s*Loaded ",
            r"^\s*Blocking ",
            r"^\s*Unpacking ",
        ],
        Some(r"^\s*Finished "),
    );
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
    let f = strip_annotated_filter(
        &[
            r"^\s*Compiling ",
            r"^\s*Downloading ",
            r"^\s*Downloaded ",
            r"^\s*Updating ",
            r"^\s*Fetching ",
            r"^\s*Fresh ",
            r"^\s*Packaging ",
            r"^\s*Verifying ",
            r"^\s*Archiving ",
            r"^\s*Locking ",
            r"^\s*Adding ",
            r"^\s*Removing ",
            r"^\s*Checking ",
            r"^\s*Documenting ",
            r"^\s*Running ",
            r"^\s*Loaded ",
            r"^\s*Blocking ",
            r"^\s*Unpacking ",
        ],
        Some(r"^\s*Finished "),
    );
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

#[test]
fn clippy_grouped_warnings_snapshot() {
    let f = group_by_rule_filter(r"^\s*-->\s*(.+:\d+)", r"#\[warn\(([^)]+)\)\]");
    let raw = "\
warning: needless pass by value
  --> src/foo.rs:12:5
   |
   = help: use a reference instead
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: needless pass by value
  --> src/bar.rs:45:10
   |
   = help: use a reference instead
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: unused import
  --> src/main.rs:5:1
   |
   = note: `#[warn(clippy::unused_imports)]` on by default

warning: `my-crate` (lib) generated 3 warnings
";
    let result = f.filter("cargo clippy", raw, 0);
    insta::assert_snapshot!(result.output);
}

#[test]
fn filter_diff_snapshot() {
    let f = git_diff_filter(500);
    let raw = "\
diff --git a/src/main.rs b/src/main.rs
index abc..def 100644
--- a/src/main.rs
+++ b/src/main.rs
+new line 1
-old line 1
diff --git a/src/lib.rs b/src/lib.rs
index ghi..jkl 100644
--- a/src/lib.rs
+++ b/src/lib.rs
+added line
";
    let result = f.filter("git diff", raw, 0);
    insta::assert_snapshot!(result.output);
}

#[test]
fn filter_status_snapshot() {
    let f = git_status_filter();
    let raw = " M src/main.rs\n M src/lib.rs\n?? new_file.txt\nA  added.rs\n";
    let result = f.filter("git status --short", raw, 0);
    insta::assert_snapshot!(result.output);
}

// --- empty input edge cases ---

#[test]
fn keep_matching_empty_input_returns_fallback() {
    let f = keep_matching_filter(&[r"->"]);
    let result = f.filter("cmd", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn strip_annotated_empty_input_returns_fallback() {
    let f = strip_annotated_filter(&[r"^\s*Compiling "], None);
    let result = f.filter("cargo build", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn test_summary_empty_input_returns_fallback() {
    let f = test_summary_filter();
    let result = f.filter("cargo test", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

#[test]
fn group_by_rule_empty_input_returns_fallback() {
    let f = group_by_rule_filter(r"^\s*-->\s*(.+:\d+)", r"#\[warn\(([^)]+)\)\]");
    let result = f.filter("cargo clippy", "", 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
}

// --- compound command matching ---

#[test]
fn compound_command_prefix_matches_last_segment() {
    // "cd /path && cargo test" extracts "cargo test" as last segment,
    // so a Prefix("cargo test") filter should apply to compound commands.
    let f = DeclarativeFilter {
        name: Arc::from("test-compound"),
        matcher: CommandMatcher::Prefix(Arc::from("cargo test")),
        strategy: CompiledStrategy::StripNoise {
            patterns: vec![Regex::new(r"^NOISE").unwrap()],
        },
    };
    assert!(f.matcher().matches("cd /path && cargo test"));
    assert!(f.matcher().matches("cargo test --lib"));
    assert!(!f.matcher().matches("cd /path && npm test"));
}

#[test]
fn compound_command_regex_match() {
    // A regex matcher can be written to match compound commands.
    let m = MatchConfig {
        exact: None,
        prefix: None,
        regex: Some(r"cargo\s+test".into()),
    };
    let matcher = compile_match(&m).unwrap();
    assert!(matcher.matches("cd /workspace && cargo test --lib"));
    assert!(matcher.matches("cargo test --workspace"));
}

// --- test_summary snapshot ---

#[test]
fn test_summary_failures_snapshot() {
    let f = test_summary_filter();
    let raw = "\
running 3 tests
test foo::test_a ... ok
test foo::test_b ... FAILED
test foo::test_c ... ok

---- foo::test_b stdout ----
thread 'foo::test_b' panicked at 'assertion `left == right` failed
  left: 1
 right: 2', src/foo.rs:42:9

failures:
foo::test_b

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 filtered out; finished in 0.02s
";
    let result = f.filter("cargo test", raw, 1);
    insta::assert_snapshot!(result.output);
}

// --- new rules: find, grep-rg, curl-wget, du-df-ps, js-test, linter ---

#[test]
fn find_filter_matches_and_truncates() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "find")
        .expect("find rule missing");
    assert!(f.matcher().matches("find . -name '*.rs'"));
    assert!(!f.matcher().matches("grep foo bar"));

    let lines: Vec<String> = (0..150).map(|i| format!("/path/file_{i}.rs")).collect();
    let raw = lines.join("\n");
    let result = f.filter("find . -name '*.rs'", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
}

#[test]
fn grep_rg_filter_matches_and_truncates() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "grep-rg")
        .expect("grep-rg rule missing");
    assert!(f.matcher().matches("grep -r foo ."));
    assert!(f.matcher().matches("rg pattern src/"));
    assert!(!f.matcher().matches("find . -name foo"));

    let lines: Vec<String> = (0..100)
        .map(|i| format!("src/file_{i}.rs:10:match here"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("rg pattern src/", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
}

#[test]
fn curl_wget_filter_strips_noise() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "curl-wget")
        .expect("curl-wget rule missing");
    assert!(f.matcher().matches("curl https://example.com"));
    assert!(f.matcher().matches("wget https://example.com/file.tar.gz"));
    assert!(!f.matcher().matches("git clone https://example.com"));

    let raw = "\
Resolving example.com... 93.184.216.34
Connecting to example.com|93.184.216.34|:443...
  % Total    % Received % Xferd
  100  1234    0  1234    0     0   5000      0 --:--:-- --:--:-- --:--:--  5000
{\"result\": \"ok\"}";
    let result = f.filter("curl https://example.com", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("{\"result\": \"ok\"}"));
    assert!(!result.output.contains("Resolving"));
    assert!(!result.output.contains("Connecting to"));
}

#[test]
fn du_df_ps_filter_matches_and_truncates() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "du-df-ps")
        .expect("du-df-ps rule missing");
    assert!(f.matcher().matches("du -sh *"));
    assert!(f.matcher().matches("df -h"));
    assert!(f.matcher().matches("ps aux"));
    assert!(f.matcher().matches("du"));
    assert!(!f.matcher().matches("docker ps"));

    let lines: Vec<String> = (0..80).map(|i| format!("{i}K\t/path/dir_{i}")).collect();
    let raw = lines.join("\n");
    let result = f.filter("du -sh *", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
}

#[test]
fn js_test_filter_matches_and_truncates() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "js-test")
        .expect("js-test rule missing");
    assert!(f.matcher().matches("jest --coverage"));
    assert!(f.matcher().matches("vitest run"));
    assert!(f.matcher().matches("npx jest src/"));
    assert!(f.matcher().matches("npx vitest --reporter verbose"));
    assert!(f.matcher().matches("mocha test/"));
    assert!(!f.matcher().matches("pytest tests/"));

    let lines: Vec<String> = (0..150)
        .map(|i| format!("  PASS src/module_{i}.test.js"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("jest --coverage", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
}

#[test]
fn linter_filter_matches_and_truncates() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "linter")
        .expect("linter rule missing");
    assert!(f.matcher().matches("eslint src/"));
    assert!(f.matcher().matches("ruff check ."));
    assert!(f.matcher().matches("mypy src/"));
    assert!(f.matcher().matches("pylint mymodule"));
    assert!(f.matcher().matches("flake8 ."));
    assert!(f.matcher().matches("npx eslint src/"));
    assert!(f.matcher().matches("python -m mypy src/"));
    assert!(f.matcher().matches("python -m pylint mymodule"));
    assert!(f.matcher().matches("python -m ruff check ."));
    assert!(!f.matcher().matches("cargo clippy"));

    let lines: Vec<String> = (0..100)
        .map(|i| format!("src/file_{i}.py:10:1: E501 line too long"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("ruff check .", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
}

// --- behavior tests for remaining TOML rules ---

#[test]
fn git_log_filter_truncates_to_head20() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "git-log")
        .expect("git-log rule missing");
    assert!(f.matcher().matches("git log --oneline"));
    assert!(!f.matcher().matches("git diff"));

    let lines: Vec<String> = (0..30)
        .map(|i| format!("abc{i:04} commit message {i}"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("git log --oneline", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("abc0000"));
    assert!(result.output.contains("abc0019"));
    assert!(!result.output.contains("abc0020"));
}

#[test]
fn git_push_filter_keeps_matching_lines() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "git-push")
        .expect("git-push rule missing");
    assert!(f.matcher().matches("git push origin main"));

    let raw = "\
Enumerating objects: 5, done.
Counting objects: 100% (5/5), done.
To github.com:user/repo.git
   abc1234..def5678  main -> main
Branch 'main' set up to track remote branch 'main' from 'origin'.
";
    let result = f.filter("git push origin main", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("->"));
    assert!(result.output.contains("To github.com"));
    assert!(result.output.contains("Branch"));
    assert!(!result.output.contains("Enumerating"));
    assert!(!result.output.contains("Counting"));
}

#[test]
fn ls_filter_strips_noise_dirs() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "ls")
        .expect("ls rule missing");
    assert!(f.matcher().matches("ls -la"));
    assert!(f.matcher().matches("ls"));
    assert!(!f.matcher().matches("lsblk"));

    let raw = "src\nnode_modules\n.git\ntarget\n__pycache__\n.venv\nCargo.toml\nREADME.md";
    let result = f.filter("ls", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("src"));
    assert!(result.output.contains("Cargo.toml"));
    assert!(!result.output.contains("node_modules"));
    assert!(!result.output.contains(".git"));
    assert!(!result.output.contains("target"));
    assert!(!result.output.contains("__pycache__"));
}

#[test]
fn docker_build_filter_strips_step_lines() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "docker-build")
        .expect("docker-build rule missing");
    assert!(f.matcher().matches("docker build -t myapp ."));

    let raw = "\
Step 1/5 : FROM ubuntu:22.04
 ---> a72860cb95fd
Step 2/5 : RUN apt-get update
Removing intermediate container b1c2d3e4f5a6
Successfully built 1a2b3c4d5e6f
Successfully tagged myapp:latest";
    let result = f.filter("docker build -t myapp .", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("Successfully built"));
    assert!(result.output.contains("Successfully tagged"));
    assert!(!result.output.contains("Step 1/5"));
    assert!(!result.output.contains("Removing intermediate container"));
}

#[test]
fn docker_compose_filter_strips_container_lines() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "docker-compose")
        .expect("docker-compose rule missing");
    assert!(f.matcher().matches("docker compose up -d"));

    let raw = "\
 Network myapp_default  Created
 Container myapp_db_1  Creating
 Container myapp_db_1  Created
 Container myapp_web_1  Starting
 Container myapp_web_1  Started
All containers up";
    let result = f.filter("docker compose up -d", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("All containers up"));
    assert!(!result.output.contains("Network myapp_default  Created"));
    assert!(!result.output.contains("Container myapp_db_1  Created"));
}

#[test]
fn npm_install_filter_strips_warn_notice() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "npm-install")
        .expect("npm-install rule missing");
    assert!(f.matcher().matches("npm install"));
    assert!(f.matcher().matches("yarn add lodash"));
    assert!(f.matcher().matches("pnpm install"));

    let raw = "\
npm warn deprecated pkg@1.0.0: Use newpkg instead
npm notice created a lockfile
added 120 packages in 3s
up to date, audited 121 packages in 1s
Packages successfully installed";
    let result = f.filter("npm install", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("Packages successfully installed"));
    assert!(!result.output.contains("npm warn"));
    assert!(!result.output.contains("npm notice"));
    assert!(!result.output.contains("added 120 packages"));
    assert!(!result.output.contains("up to date"));
}

#[test]
fn pip_install_filter_strips_collecting_lines() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "pip-install")
        .expect("pip-install rule missing");
    assert!(f.matcher().matches("pip install requests"));
    assert!(f.matcher().matches("pip3 install -r requirements.txt"));

    let raw = "\
  Collecting requests
  Downloading requests-2.31.0-py3-none-any.whl (62 kB)
  Installing collected packages: requests
  Using cached certifi-2024.2.2-py3-none-any.whl
Successfully installed requests-2.31.0";
    let result = f.filter("pip install requests", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("Successfully installed"));
    assert!(!result.output.contains("Collecting"));
    assert!(!result.output.contains("Downloading"));
    assert!(!result.output.contains("Using cached"));
    assert!(!result.output.contains("Installing collected"));
}

#[test]
fn make_filter_truncates_long_output() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "make")
        .expect("make rule missing");
    assert!(f.matcher().matches("make all"));
    assert!(f.matcher().matches("make -j4 build"));

    let lines: Vec<String> = (0..100)
        .map(|i| format!("gcc -o obj/file_{i}.o src/file_{i}.c"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("make all", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("file_0.o"));
    assert!(result.output.contains("file_99.o"));
}

#[test]
fn pytest_filter_truncates_long_output() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "pytest")
        .expect("pytest rule missing");
    assert!(f.matcher().matches("pytest tests/"));
    assert!(f.matcher().matches("python -m pytest -v"));

    let lines: Vec<String> = (0..150)
        .map(|i| format!("tests/test_module_{i}.py::test_fn PASSED"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("pytest tests/", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("test_module_0"));
    assert!(result.output.contains("test_module_149"));
}

#[test]
fn go_test_filter_truncates_long_output() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "go-test")
        .expect("go-test rule missing");
    assert!(f.matcher().matches("go test ./..."));
    assert!(f.matcher().matches("go test -v -run TestFoo ./pkg/..."));

    let lines: Vec<String> = (0..100)
        .map(|i| format!("--- PASS: TestFunc{i} (0.00s)"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("go test ./...", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("TestFunc0"));
    assert!(result.output.contains("TestFunc99"));
}

#[test]
fn terraform_plan_filter_truncates_long_output() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "terraform-plan")
        .expect("terraform-plan rule missing");
    assert!(f.matcher().matches("terraform plan -out=tfplan"));
    assert!(f.matcher().matches("terraform apply tfplan"));
    assert!(!f.matcher().matches("terraform init"));

    let lines: Vec<String> = (0..80)
        .map(|i| format!("  + resource \"aws_instance\" \"web_{i}\" {{"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("terraform plan", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("web_0"));
    assert!(result.output.contains("web_79"));
}

#[test]
fn kubectl_get_filter_truncates_long_output() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "kubectl-get")
        .expect("kubectl-get rule missing");
    assert!(f.matcher().matches("kubectl get pods -n default"));
    assert!(f.matcher().matches("kubectl describe node worker-1"));
    assert!(!f.matcher().matches("kubectl apply -f manifest.yaml"));

    let lines: Vec<String> = (0..70)
        .map(|i| format!("pod-{i:03}   1/1   Running   0   5d"))
        .collect();
    let raw = lines.join("\n");
    let result = f.filter("kubectl get pods", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Partial);
    assert!(result.output.contains("lines omitted"));
    assert!(result.output.contains("pod-000"));
    assert!(result.output.contains("pod-069"));
}

#[test]
fn brew_install_filter_strips_download_lines() {
    let filters = load_declarative_filters(None);
    let f = filters
        .iter()
        .find(|f| f.name() == "brew-install")
        .expect("brew-install rule missing");
    assert!(f.matcher().matches("brew install ripgrep"));
    assert!(f.matcher().matches("brew upgrade git"));
    assert!(!f.matcher().matches("brew list"));

    let raw = "\
==> Downloading https://ghcr.io/v2/homebrew/core/ripgrep/manifests/14.1.0
==> Fetching ripgrep
==> Installing ripgrep
==> Pouring ripgrep--14.1.0.arm64_sonoma.bottle.tar.gz
Already downloaded: /Users/user/Library/Caches/Homebrew/ripgrep-14.1.0.bottle.tar.gz
ripgrep installed successfully";
    let result = f.filter("brew install ripgrep", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("ripgrep installed successfully"));
    assert!(!result.output.contains("==> Downloading"));
    assert!(!result.output.contains("==> Fetching"));
    assert!(!result.output.contains("==> Pouring"));
    assert!(!result.output.contains("Already downloaded"));
}

// --- edge case tests ---

#[test]
fn truncate_exactly_at_threshold_returns_fallback() {
    let f = truncate_filter(10, 5, 5);
    let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
    let raw = lines.join("\n");
    let result = f.filter("cmd", &raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Fallback);
    assert!(!result.output.contains("lines omitted"));
}

#[test]
fn dedup_cap_hit_reports_capped() {
    let f = DeclarativeFilter {
        name: Arc::from("test-dedup-capped"),
        matcher: CommandMatcher::Prefix(Arc::from("journalctl")),
        strategy: CompiledStrategy::Dedup {
            normalize_patterns: vec![],
            max_unique_patterns: 3,
        },
    };
    // 6 unique lines → exceeds cap of 3
    let raw = "line_a\nline_b\nline_c\nline_d\nline_e\nline_f";
    let result = f.filter("journalctl", raw, 0);
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("capped at 3"));
}

#[test]
fn strip_annotated_with_summary_pattern_but_no_summary_line() {
    let f = strip_annotated_filter(&[r"^\s*Compiling "], Some(r"^\s*Finished "));
    // Input has noise lines but no "Finished" summary line
    let raw = "\
Compiling foo v1.0
Compiling bar v2.0
error: build failed";
    let result = f.filter("cargo build", raw, 0);
    // Must not panic; noise was removed, so Full confidence expected
    assert_eq!(result.confidence, FilterConfidence::Full);
    assert!(result.output.contains("noise lines removed"));
    assert!(result.output.contains("error: build failed"));
}

use proptest::prelude::*;

proptest! {
    #[test]
    fn declarative_filter_never_panics_strip_noise(
        input in ".*",
        cmd in ".*",
        exit_code in -1i32..=255,
    ) {
        let f = strip_noise_filter(&[r"^noise", r"^\s*$"]);
        let _ = f.filter(&cmd, &input, exit_code);
    }

    #[test]
    fn declarative_filter_never_panics_truncate(
        input in ".*",
        cmd in ".*",
        exit_code in -1i32..=255,
    ) {
        let f = truncate_filter(10, 3, 3);
        let _ = f.filter(&cmd, &input, exit_code);
    }

    #[test]
    fn declarative_filter_never_panics_test_summary(
        input in ".*",
        cmd in ".*",
        exit_code in -1i32..=255,
    ) {
        let f = test_summary_filter();
        let _ = f.filter(&cmd, &input, exit_code);
    }

    #[test]
    fn declarative_filter_never_panics_dedup(
        input in ".*",
        cmd in ".*",
        exit_code in -1i32..=255,
    ) {
        let f = dedup_filter();
        let _ = f.filter(&cmd, &input, exit_code);
    }
}
