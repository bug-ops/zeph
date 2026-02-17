use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::LazyLock;

use regex::Regex;

use super::{
    ClippyFilterConfig, CommandMatcher, FilterConfidence, FilterResult, OutputFilter,
    cargo_build::is_cargo_noise, make_result,
};

static CLIPPY_MATCHER: LazyLock<CommandMatcher> = LazyLock::new(|| {
    CommandMatcher::Custom(Box::new(|cmd| {
        let c = cmd.to_lowercase();
        let tokens: Vec<&str> = c.split_whitespace().collect();
        tokens.first() == Some(&"cargo") && tokens.iter().skip(1).any(|t| *t == "clippy")
    }))
});

static LINT_RULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\[warn\(([^)]+)\)\]").unwrap());

static LOCATION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*-->\s*(.+:\d+)").unwrap());

pub struct ClippyFilter;

impl ClippyFilter {
    #[must_use]
    pub fn new(_config: ClippyFilterConfig) -> Self {
        Self
    }
}

impl OutputFilter for ClippyFilter {
    fn name(&self) -> &'static str {
        "clippy"
    }

    fn matcher(&self) -> &CommandMatcher {
        &CLIPPY_MATCHER
    }

    fn filter(&self, _command: &str, raw_output: &str, exit_code: i32) -> FilterResult {
        let has_error = raw_output.contains("error[") || raw_output.contains("error:");
        if has_error && exit_code != 0 {
            return make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            );
        }

        let mut warnings: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut pending_location: Option<String> = None;

        for line in raw_output.lines() {
            if let Some(caps) = LOCATION_RE.captures(line) {
                pending_location = Some(caps[1].to_owned());
            }

            if let Some(caps) = LINT_RULE_RE.captures(line) {
                let rule = caps[1].to_owned();
                if let Some(loc) = pending_location.take() {
                    warnings.entry(rule).or_default().push(loc);
                }
            }
        }

        if warnings.is_empty() {
            let kept: Vec<&str> = raw_output.lines().filter(|l| !is_cargo_noise(l)).collect();
            if kept.len() < raw_output.lines().count() {
                let output = kept.join("\n");
                return make_result(raw_output, output, FilterConfidence::Partial);
            }
            return make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            );
        }

        let total: usize = warnings.values().map(Vec::len).sum();
        let rules = warnings.len();
        let mut output = String::new();

        for (rule, locations) in &warnings {
            let count = locations.len();
            let label = if count == 1 { "warning" } else { "warnings" };
            let _ = writeln!(output, "{rule} ({count} {label}):");
            for loc in locations {
                let _ = writeln!(output, "  {loc}");
            }
            output.push('\n');
        }
        let _ = write!(output, "{total} warnings total ({rules} rules)");

        make_result(raw_output, output, FilterConfidence::Full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter() -> ClippyFilter {
        ClippyFilter::new(ClippyFilterConfig::default())
    }

    #[test]
    fn matches_clippy() {
        let f = make_filter();
        assert!(f.matcher().matches("cargo clippy --workspace"));
        assert!(f.matcher().matches("cargo clippy -- -D warnings"));
        assert!(f.matcher().matches("cargo +nightly clippy"));
        assert!(!f.matcher().matches("cargo build"));
        assert!(!f.matcher().matches("cargo test"));
    }

    #[test]
    fn filter_groups_warnings() {
        let f = make_filter();
        let raw = "\
warning: needless pass by value
  --> src/foo.rs:12:5
   |
   = help: ...
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: needless pass by value
  --> src/bar.rs:45:10
   |
   = help: ...
   = note: `#[warn(clippy::needless_pass_by_value)]` on by default

warning: unused import
  --> src/main.rs:5:1
   |
   = note: `#[warn(clippy::unused_imports)]` on by default

warning: `my-crate` (lib) generated 3 warnings
";
        let result = f.filter("cargo clippy", raw, 0);
        assert!(
            result
                .output
                .contains("clippy::needless_pass_by_value (2 warnings):")
        );
        assert!(result.output.contains("src/foo.rs:12"));
        assert!(result.output.contains("src/bar.rs:45"));
        assert!(
            result
                .output
                .contains("clippy::unused_imports (1 warning):")
        );
        assert!(result.output.contains("3 warnings total (2 rules)"));
        assert_eq!(result.confidence, FilterConfidence::Full);
    }

    #[test]
    fn filter_error_preserves_full() {
        let f = make_filter();
        let raw = "error[E0308]: mismatched types\n  --> src/main.rs:10:5\nfull details here";
        let result = f.filter("cargo clippy", raw, 1);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn filter_no_warnings_strips_noise() {
        let f = make_filter();
        let raw = "Checking my-crate v0.1.0\n    Finished dev [unoptimized] target(s)";
        let result = f.filter("cargo clippy", raw, 0);
        assert!(result.output.is_empty());
        assert_eq!(result.confidence, FilterConfidence::Partial);
    }
}
