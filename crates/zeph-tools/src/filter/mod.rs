//! Command-aware output filtering pipeline.

pub(crate) mod cargo_build;
mod clippy;
mod dir_listing;
mod git;
mod log_dedup;
pub mod security;
mod test_output;

use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::Deserialize;

pub use self::cargo_build::CargoBuildFilter;
pub use self::clippy::ClippyFilter;
pub use self::dir_listing::DirListingFilter;
pub use self::git::GitFilter;
pub use self::log_dedup::LogDedupFilter;
pub use self::test_output::TestOutputFilter;

// ---------------------------------------------------------------------------
// FilterConfidence (#440)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterConfidence {
    Full,
    Partial,
    Fallback,
}

// ---------------------------------------------------------------------------
// FilterResult
// ---------------------------------------------------------------------------

/// Result of applying a filter to tool output.
pub struct FilterResult {
    pub output: String,
    pub raw_chars: usize,
    pub filtered_chars: usize,
    pub raw_lines: usize,
    pub filtered_lines: usize,
    pub confidence: FilterConfidence,
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

// ---------------------------------------------------------------------------
// CommandMatcher (#439)
// ---------------------------------------------------------------------------

pub enum CommandMatcher {
    Exact(&'static str),
    Prefix(&'static str),
    Regex(regex::Regex),
    Custom(Box<dyn Fn(&str) -> bool + Send + Sync>),
}

impl CommandMatcher {
    #[must_use]
    pub fn matches(&self, command: &str) -> bool {
        self.matches_single(command)
            || extract_last_command(command).is_some_and(|last| self.matches_single(last))
    }

    fn matches_single(&self, command: &str) -> bool {
        match self {
            Self::Exact(s) => command == *s,
            Self::Prefix(s) => command.starts_with(s),
            Self::Regex(re) => re.is_match(command),
            Self::Custom(f) => f(command),
        }
    }
}

/// Extract the last command segment from compound shell expressions
/// like `cd /path && cargo test` or `cmd1 ; cmd2`. Strips trailing
/// redirections and pipes (e.g. `2>&1 | tail -50`).
fn extract_last_command(command: &str) -> Option<&str> {
    let last = command
        .rsplit("&&")
        .next()
        .or_else(|| command.rsplit(';').next())?;
    let last = last.trim();
    if last == command.trim() {
        return None;
    }
    // Strip trailing pipe chain and redirections: take content before first `|` or `2>`
    let last = last.split('|').next().unwrap_or(last);
    let last = last.split("2>").next().unwrap_or(last);
    let trimmed = last.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

impl std::fmt::Debug for CommandMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(s) => write!(f, "Exact({s:?})"),
            Self::Prefix(s) => write!(f, "Prefix({s:?})"),
            Self::Regex(re) => write!(f, "Regex({:?})", re.as_str()),
            Self::Custom(_) => write!(f, "Custom(...)"),
        }
    }
}

// ---------------------------------------------------------------------------
// OutputFilter trait
// ---------------------------------------------------------------------------

/// Command-aware output filter.
pub trait OutputFilter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matcher(&self) -> &CommandMatcher;
    fn filter(&self, command: &str, raw_output: &str, exit_code: i32) -> FilterResult;
}

// ---------------------------------------------------------------------------
// FilterPipeline (#441)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct FilterPipeline<'a> {
    stages: Vec<&'a dyn OutputFilter>,
}

impl<'a> FilterPipeline<'a> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, filter: &'a dyn OutputFilter) {
        self.stages.push(filter);
    }

    #[must_use]
    pub fn run(&self, command: &str, output: &str, exit_code: i32) -> FilterResult {
        let initial_len = output.len();
        let mut current = output.to_owned();
        let mut worst = FilterConfidence::Full;

        for stage in &self.stages {
            let result = stage.filter(command, &current, exit_code);
            worst = worse_confidence(worst, result.confidence);
            current = result.output;
        }

        FilterResult {
            raw_chars: initial_len,
            filtered_chars: current.len(),
            raw_lines: count_lines(output),
            filtered_lines: count_lines(&current),
            output: current,
            confidence: worst,
        }
    }
}

#[must_use]
pub fn worse_confidence(a: FilterConfidence, b: FilterConfidence) -> FilterConfidence {
    match (a, b) {
        (FilterConfidence::Fallback, _) | (_, FilterConfidence::Fallback) => {
            FilterConfidence::Fallback
        }
        (FilterConfidence::Partial, _) | (_, FilterConfidence::Partial) => {
            FilterConfidence::Partial
        }
        _ => FilterConfidence::Full,
    }
}

// ---------------------------------------------------------------------------
// FilterMetrics (#442)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FilterMetrics {
    pub total_commands: u64,
    pub filtered_commands: u64,
    pub skipped_commands: u64,
    pub raw_chars_total: u64,
    pub filtered_chars_total: u64,
    pub confidence_counts: [u64; 3],
}

impl FilterMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            total_commands: 0,
            filtered_commands: 0,
            skipped_commands: 0,
            raw_chars_total: 0,
            filtered_chars_total: 0,
            confidence_counts: [0; 3],
        }
    }

    pub fn record(&mut self, result: &FilterResult) {
        self.total_commands += 1;
        if result.filtered_chars < result.raw_chars {
            self.filtered_commands += 1;
        } else {
            self.skipped_commands += 1;
        }
        self.raw_chars_total += result.raw_chars as u64;
        self.filtered_chars_total += result.filtered_chars as u64;
        let idx = match result.confidence {
            FilterConfidence::Full => 0,
            FilterConfidence::Partial => 1,
            FilterConfidence::Fallback => 2,
        };
        self.confidence_counts[idx] += 1;
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn savings_pct(&self) -> f64 {
        if self.raw_chars_total == 0 {
            return 0.0;
        }
        (1.0 - self.filtered_chars_total as f64 / self.raw_chars_total as f64) * 100.0
    }
}

impl Default for FilterMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FilterConfig (#444)
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_max_failures() -> usize {
    10
}

fn default_stack_trace_lines() -> usize {
    50
}

fn default_max_log_entries() -> usize {
    20
}

fn default_max_diff_lines() -> usize {
    500
}

/// Configuration for output filters.
#[derive(Debug, Clone, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub test: TestFilterConfig,

    #[serde(default)]
    pub git: GitFilterConfig,

    #[serde(default)]
    pub clippy: ClippyFilterConfig,

    #[serde(default)]
    pub cargo_build: CargoBuildFilterConfig,

    #[serde(default)]
    pub dir_listing: DirListingFilterConfig,

    #[serde(default)]
    pub log_dedup: LogDedupFilterConfig,

    #[serde(default)]
    pub security: SecurityFilterConfig,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            test: TestFilterConfig::default(),
            git: GitFilterConfig::default(),
            clippy: ClippyFilterConfig::default(),
            cargo_build: CargoBuildFilterConfig::default(),
            dir_listing: DirListingFilterConfig::default(),
            log_dedup: LogDedupFilterConfig::default(),
            security: SecurityFilterConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_failures")]
    pub max_failures: usize,
    #[serde(default = "default_stack_trace_lines")]
    pub truncate_stack_trace: usize,
}

impl Default for TestFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_failures: default_max_failures(),
            truncate_stack_trace: default_stack_trace_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_log_entries")]
    pub max_log_entries: usize,
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: usize,
}

impl Default for GitFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_log_entries: default_max_log_entries(),
            max_diff_lines: default_max_diff_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClippyFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ClippyFilterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CargoBuildFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CargoBuildFilterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DirListingFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for DirListingFilterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogDedupFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for LogDedupFilterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecurityFilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

impl Default for SecurityFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_patterns: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// OutputFilterRegistry
// ---------------------------------------------------------------------------

/// Registry of filters with pipeline support, security whitelist, and metrics.
pub struct OutputFilterRegistry {
    filters: Vec<Box<dyn OutputFilter>>,
    enabled: bool,
    security_enabled: bool,
    extra_security_patterns: Vec<regex::Regex>,
    metrics: Mutex<FilterMetrics>,
}

impl std::fmt::Debug for OutputFilterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputFilterRegistry")
            .field("enabled", &self.enabled)
            .field("filter_count", &self.filters.len())
            .finish_non_exhaustive()
    }
}

impl OutputFilterRegistry {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            filters: Vec::new(),
            enabled,
            security_enabled: true,
            extra_security_patterns: Vec::new(),
            metrics: Mutex::new(FilterMetrics::new()),
        }
    }

    pub fn register(&mut self, filter: Box<dyn OutputFilter>) {
        self.filters.push(filter);
    }

    #[must_use]
    pub fn default_filters(config: &FilterConfig) -> Self {
        let mut r = Self {
            filters: Vec::new(),
            enabled: config.enabled,
            security_enabled: config.security.enabled,
            extra_security_patterns: security::compile_extra_patterns(
                &config.security.extra_patterns,
            ),
            metrics: Mutex::new(FilterMetrics::new()),
        };
        if config.test.enabled {
            r.register(Box::new(TestOutputFilter::new(config.test.clone())));
        }
        if config.clippy.enabled {
            r.register(Box::new(ClippyFilter::new(config.clippy.clone())));
        }
        if config.cargo_build.enabled {
            r.register(Box::new(CargoBuildFilter::new(config.cargo_build.clone())));
        }
        if config.git.enabled {
            r.register(Box::new(GitFilter::new(config.git.clone())));
        }
        if config.dir_listing.enabled {
            r.register(Box::new(DirListingFilter::new(config.dir_listing.clone())));
        }
        if config.log_dedup.enabled {
            r.register(Box::new(LogDedupFilter::new(config.log_dedup.clone())));
        }
        r
    }

    #[must_use]
    pub fn apply(&self, command: &str, raw_output: &str, exit_code: i32) -> Option<FilterResult> {
        if !self.enabled {
            return None;
        }

        let matching: Vec<&dyn OutputFilter> = self
            .filters
            .iter()
            .filter(|f| f.matcher().matches(command))
            .map(AsRef::as_ref)
            .collect();

        if matching.is_empty() {
            return None;
        }

        let mut result = if matching.len() == 1 {
            matching[0].filter(command, raw_output, exit_code)
        } else {
            let mut pipeline = FilterPipeline::new();
            for f in &matching {
                pipeline.push(*f);
            }
            pipeline.run(command, raw_output, exit_code)
        };

        if self.security_enabled {
            security::append_security_warnings(
                &mut result.output,
                raw_output,
                &self.extra_security_patterns,
            );
        }

        self.record_metrics(&result);
        Some(result)
    }

    fn record_metrics(&self, result: &FilterResult) {
        if let Ok(mut m) = self.metrics.lock() {
            m.record(result);
            if m.total_commands % 50 == 0 {
                tracing::debug!(
                    total = m.total_commands,
                    filtered = m.filtered_commands,
                    savings_pct = format!("{:.1}", m.savings_pct()),
                    "filter metrics"
                );
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> FilterMetrics {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b[()][A-B0-2]").unwrap());

/// Strip only ANSI escape sequences, preserving newlines and whitespace.
#[must_use]
pub fn strip_ansi(raw: &str) -> String {
    ANSI_RE.replace_all(raw, "").into_owned()
}

/// Strip ANSI escape sequences, carriage-return progress bars, and collapse blank lines.
#[must_use]
pub fn sanitize_output(raw: &str) -> String {
    let no_ansi = ANSI_RE.replace_all(raw, "");

    let mut result = String::with_capacity(no_ansi.len());
    let mut prev_blank = false;

    for line in no_ansi.lines() {
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

fn count_lines(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

fn make_result(raw: &str, output: String, confidence: FilterConfidence) -> FilterResult {
    let filtered_chars = output.len();
    FilterResult {
        raw_lines: count_lines(raw),
        filtered_lines: count_lines(&output),
        output,
        raw_chars: raw.len(),
        filtered_chars,
        confidence,
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
            raw_lines: 0,
            filtered_lines: 0,
            confidence: FilterConfidence::Full,
        };
        assert!((r.savings_pct() - 80.0).abs() < 0.01);
    }

    #[test]
    fn filter_result_savings_pct_zero_raw() {
        let r = FilterResult {
            output: String::new(),
            raw_chars: 0,
            filtered_chars: 0,
            raw_lines: 0,
            filtered_lines: 0,
            confidence: FilterConfidence::Full,
        };
        assert!((r.savings_pct()).abs() < 0.01);
    }

    #[test]
    fn count_lines_helper() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("one"), 1);
        assert_eq!(count_lines("one\ntwo\nthree"), 3);
        assert_eq!(count_lines("trailing\n"), 1);
    }

    #[test]
    fn make_result_counts_lines() {
        let raw = "line1\nline2\nline3\nline4\nline5";
        let filtered = "line1\nline3".to_owned();
        let r = make_result(raw, filtered, FilterConfidence::Full);
        assert_eq!(r.raw_lines, 5);
        assert_eq!(r.filtered_lines, 2);
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
        let r = OutputFilterRegistry::default_filters(&FilterConfig::default());
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

    #[test]
    fn filter_config_deserialize_minimal() {
        let toml_str = "enabled = true";
        let c: FilterConfig = toml::from_str(toml_str).unwrap();
        assert!(c.enabled);
        assert!(c.test.enabled);
        assert!(c.git.enabled);
        assert!(c.clippy.enabled);
        assert!(c.security.enabled);
    }

    #[test]
    fn filter_config_deserialize_full() {
        let toml_str = r#"
enabled = true

[test]
enabled = true
max_failures = 5
truncate_stack_trace = 30

[git]
enabled = true
max_log_entries = 10
max_diff_lines = 200

[clippy]
enabled = true

[security]
enabled = true
extra_patterns = ["TODO: security review"]
"#;
        let c: FilterConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(c.test.max_failures, 5);
        assert_eq!(c.test.truncate_stack_trace, 30);
        assert_eq!(c.git.max_log_entries, 10);
        assert_eq!(c.git.max_diff_lines, 200);
        assert!(c.clippy.enabled);
        assert_eq!(c.security.extra_patterns, vec!["TODO: security review"]);
    }

    #[test]
    fn disabled_filter_excluded_from_registry() {
        let config = FilterConfig {
            test: TestFilterConfig {
                enabled: false,
                ..TestFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        let r = OutputFilterRegistry::default_filters(&config);
        assert!(
            r.apply(
                "cargo test",
                "test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out",
                0
            )
            .is_none()
        );
    }

    // CommandMatcher tests
    #[test]
    fn command_matcher_exact() {
        let m = CommandMatcher::Exact("ls");
        assert!(m.matches("ls"));
        assert!(!m.matches("ls -la"));
    }

    #[test]
    fn command_matcher_prefix() {
        let m = CommandMatcher::Prefix("git ");
        assert!(m.matches("git status"));
        assert!(!m.matches("github"));
    }

    #[test]
    fn command_matcher_regex() {
        let m = CommandMatcher::Regex(Regex::new(r"^cargo\s+test").unwrap());
        assert!(m.matches("cargo test"));
        assert!(m.matches("cargo test --lib"));
        assert!(!m.matches("cargo build"));
    }

    #[test]
    fn command_matcher_custom() {
        let m = CommandMatcher::Custom(Box::new(|cmd| cmd.contains("hello")));
        assert!(m.matches("say hello world"));
        assert!(!m.matches("goodbye"));
    }

    #[test]
    fn command_matcher_compound_cd_and() {
        let m = CommandMatcher::Prefix("cargo ");
        assert!(m.matches("cd /some/path && cargo test --workspace --lib"));
        assert!(m.matches("cd /path && cargo clippy --workspace -- -D warnings 2>&1"));
    }

    #[test]
    fn command_matcher_compound_with_pipe() {
        let m = CommandMatcher::Custom(Box::new(|cmd| cmd.split_whitespace().any(|t| t == "test")));
        assert!(m.matches("cd /path && cargo test --workspace --lib 2>&1 | tail -80"));
    }

    #[test]
    fn command_matcher_compound_no_false_positive() {
        let m = CommandMatcher::Exact("ls");
        assert!(!m.matches("cd /path && cargo test"));
    }

    #[test]
    fn extract_last_command_basic() {
        assert_eq!(
            extract_last_command("cd /path && cargo test --lib"),
            Some("cargo test --lib")
        );
        assert_eq!(
            extract_last_command("cd /p && cargo clippy 2>&1 | tail -20"),
            Some("cargo clippy")
        );
        assert!(extract_last_command("cargo test").is_none());
    }

    // FilterConfidence derives
    #[test]
    fn filter_confidence_derives() {
        let a = FilterConfidence::Full;
        let b = a;
        assert_eq!(a, b);
        let _ = format!("{a:?}");
        let mut set = std::collections::HashSet::new();
        set.insert(a);
    }

    // FilterMetrics tests
    #[test]
    fn filter_metrics_new_zeros() {
        let m = FilterMetrics::new();
        assert_eq!(m.total_commands, 0);
        assert_eq!(m.filtered_commands, 0);
        assert_eq!(m.skipped_commands, 0);
        assert_eq!(m.confidence_counts, [0; 3]);
    }

    #[test]
    fn filter_metrics_record() {
        let mut m = FilterMetrics::new();
        let r = FilterResult {
            output: "short".into(),
            raw_chars: 100,
            filtered_chars: 5,
            raw_lines: 10,
            filtered_lines: 1,
            confidence: FilterConfidence::Full,
        };
        m.record(&r);
        assert_eq!(m.total_commands, 1);
        assert_eq!(m.filtered_commands, 1);
        assert_eq!(m.skipped_commands, 0);
        assert_eq!(m.confidence_counts[0], 1);
    }

    #[test]
    fn filter_metrics_savings_pct() {
        let mut m = FilterMetrics::new();
        m.raw_chars_total = 1000;
        m.filtered_chars_total = 200;
        assert!((m.savings_pct() - 80.0).abs() < 0.01);
    }

    #[test]
    fn registry_metrics_updated() {
        let r = OutputFilterRegistry::default_filters(&FilterConfig::default());
        let _ = r.apply(
            "cargo test",
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out",
            0,
        );
        let m = r.metrics();
        assert_eq!(m.total_commands, 1);
    }

    // Pipeline tests
    #[test]
    fn pipeline_single_stage() {
        let config = FilterConfig::default();
        let filter = TestOutputFilter::new(config.test.clone());
        let mut pipeline = FilterPipeline::new();
        pipeline.push(&filter);
        let result = pipeline.run(
            "cargo test",
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out",
            0,
        );
        assert!(result.output.contains("5 passed"));
    }

    #[test]
    fn confidence_aggregation() {
        assert_eq!(
            worse_confidence(FilterConfidence::Full, FilterConfidence::Partial),
            FilterConfidence::Partial
        );
        assert_eq!(
            worse_confidence(FilterConfidence::Full, FilterConfidence::Fallback),
            FilterConfidence::Fallback
        );
        assert_eq!(
            worse_confidence(FilterConfidence::Partial, FilterConfidence::Fallback),
            FilterConfidence::Fallback
        );
        assert_eq!(
            worse_confidence(FilterConfidence::Full, FilterConfidence::Full),
            FilterConfidence::Full
        );
    }

    // Helper filter for pipeline integration test: replaces a word.
    struct ReplaceFilter {
        from: &'static str,
        to: &'static str,
        confidence: FilterConfidence,
    }

    static MATCH_ALL: LazyLock<CommandMatcher> =
        LazyLock::new(|| CommandMatcher::Custom(Box::new(|_| true)));

    impl OutputFilter for ReplaceFilter {
        fn name(&self) -> &'static str {
            "replace"
        }
        fn matcher(&self) -> &CommandMatcher {
            &MATCH_ALL
        }
        fn filter(&self, _cmd: &str, raw: &str, _exit: i32) -> FilterResult {
            let output = raw.replace(self.from, self.to);
            make_result(raw, output, self.confidence)
        }
    }

    #[test]
    fn pipeline_multi_stage_chains_and_aggregates() {
        let f1 = ReplaceFilter {
            from: "hello",
            to: "world",
            confidence: FilterConfidence::Full,
        };
        let f2 = ReplaceFilter {
            from: "world",
            to: "DONE",
            confidence: FilterConfidence::Partial,
        };

        let mut pipeline = FilterPipeline::new();
        pipeline.push(&f1);
        pipeline.push(&f2);

        let result = pipeline.run("test", "say hello there", 0);
        // f1: "hello" -> "world", f2: "world" -> "DONE"
        assert_eq!(result.output, "say DONE there");
        assert_eq!(result.confidence, FilterConfidence::Partial);
        assert_eq!(result.raw_chars, "say hello there".len());
        assert_eq!(result.filtered_chars, "say DONE there".len());
    }

    #[test]
    fn registry_pipeline_with_two_matching_filters() {
        let mut reg = OutputFilterRegistry::new(true);
        reg.register(Box::new(ReplaceFilter {
            from: "aaa",
            to: "bbb",
            confidence: FilterConfidence::Full,
        }));
        reg.register(Box::new(ReplaceFilter {
            from: "bbb",
            to: "ccc",
            confidence: FilterConfidence::Fallback,
        }));

        let result = reg.apply("test", "aaa", 0).unwrap();
        // Both match "test" via MATCH_ALL. Pipeline: "aaa" -> "bbb" -> "ccc"
        assert_eq!(result.output, "ccc");
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }
}
