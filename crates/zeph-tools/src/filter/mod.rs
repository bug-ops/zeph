//! Command-aware output filtering pipeline.

mod clippy;
mod dir_listing;
mod git;
mod log_dedup;
mod test_output;

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

pub use self::clippy::ClippyFilter;
pub use self::dir_listing::DirListingFilter;
pub use self::git::GitFilter;
pub use self::log_dedup::LogDedupFilter;
pub use self::test_output::TestOutputFilter;

/// Result of applying a filter to tool output.
pub struct FilterResult {
    pub output: String,
    pub raw_chars: usize,
    pub filtered_chars: usize,
}

impl FilterResult {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn savings_pct(&self) -> f64 {
        if self.raw_chars == 0 {
            return 0.0;
        }
        (1.0 - self.filtered_chars as f64 / self.raw_chars as f64) * 100.0
    }
}

/// Command-aware output filter.
pub trait OutputFilter: Send + Sync {
    fn matches(&self, command: &str) -> bool;
    fn filter(&self, command: &str, raw_output: &str, exit_code: i32) -> FilterResult;
}

/// Registry of filters. First match wins; no match = passthrough.
pub struct OutputFilterRegistry {
    filters: Vec<Box<dyn OutputFilter>>,
    enabled: bool,
}

impl std::fmt::Debug for OutputFilterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputFilterRegistry")
            .field("enabled", &self.enabled)
            .field("filter_count", &self.filters.len())
            .finish()
    }
}

impl OutputFilterRegistry {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            filters: Vec::new(),
            enabled,
        }
    }

    pub fn register(&mut self, filter: Box<dyn OutputFilter>) {
        self.filters.push(filter);
    }

    #[must_use]
    pub fn default_filters() -> Self {
        let mut r = Self::new(true);
        r.register(Box::new(TestOutputFilter));
        r.register(Box::new(ClippyFilter));
        r.register(Box::new(GitFilter));
        r.register(Box::new(DirListingFilter));
        r.register(Box::new(LogDedupFilter));
        r
    }

    #[must_use]
    pub fn apply(&self, command: &str, raw_output: &str, exit_code: i32) -> Option<FilterResult> {
        if !self.enabled {
            return None;
        }
        for f in &self.filters {
            if f.matches(command) {
                return Some(f.filter(command, raw_output, exit_code));
            }
        }
        None
    }
}

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());

/// Strip ANSI escape sequences, carriage-return progress bars, and collapse blank lines.
#[must_use]
pub fn sanitize_output(raw: &str) -> String {
    let no_ansi = ANSI_RE.replace_all(raw, "");

    let mut result = String::with_capacity(no_ansi.len());
    let mut prev_blank = false;

    for line in no_ansi.lines() {
        // Strip carriage-return overwrites (progress bars)
        let clean = if line.contains('\r') {
            line.rsplit('\r').next().unwrap_or("")
        } else {
            line
        };

        let is_blank = clean.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        prev_blank = is_blank;

        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(clean);
    }
    result
}

fn default_true() -> bool {
    true
}

/// Configuration for output filters.
#[derive(Debug, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn make_result(raw: &str, output: String) -> FilterResult {
    let filtered_chars = output.len();
    FilterResult {
        output,
        raw_chars: raw.len(),
        filtered_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi() {
        let input = "\x1b[32mOK\x1b[0m test passed";
        assert_eq!(sanitize_output(input), "OK test passed");
    }

    #[test]
    fn sanitize_strips_cr_progress() {
        let input = "Downloading... 50%\rDownloading... 100%";
        assert_eq!(sanitize_output(input), "Downloading... 100%");
    }

    #[test]
    fn sanitize_collapses_blank_lines() {
        let input = "line1\n\n\n\nline2";
        assert_eq!(sanitize_output(input), "line1\n\nline2");
    }

    #[test]
    fn sanitize_preserves_crlf_content() {
        let input = "line1\r\nline2\r\n";
        let result = sanitize_output(input);
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
    }

    #[test]
    fn filter_result_savings_pct() {
        let r = FilterResult {
            output: String::new(),
            raw_chars: 1000,
            filtered_chars: 200,
        };
        assert!((r.savings_pct() - 80.0).abs() < 0.01);
    }

    #[test]
    fn filter_result_savings_pct_zero_raw() {
        let r = FilterResult {
            output: String::new(),
            raw_chars: 0,
            filtered_chars: 0,
        };
        assert!((r.savings_pct()).abs() < 0.01);
    }

    #[test]
    fn registry_disabled_returns_none() {
        let r = OutputFilterRegistry::new(false);
        assert!(r.apply("cargo test", "output", 0).is_none());
    }

    #[test]
    fn registry_no_match_returns_none() {
        let r = OutputFilterRegistry::new(true);
        assert!(r.apply("some-unknown-cmd", "output", 0).is_none());
    }

    #[test]
    fn registry_default_has_filters() {
        let r = OutputFilterRegistry::default_filters();
        assert!(
            r.apply(
                "cargo test",
                "test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out",
                0
            )
            .is_some()
        );
    }

    #[test]
    fn filter_config_default_enabled() {
        let c = FilterConfig::default();
        assert!(c.enabled);
    }

    #[test]
    fn filter_config_deserialize() {
        let toml_str = "enabled = false";
        let c: FilterConfig = toml::from_str(toml_str).unwrap();
        assert!(!c.enabled);
    }
}
