// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Command-aware output filtering pipeline.

pub(crate) mod declarative;
pub mod security;

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use regex::Regex;
use serde::{Deserialize, Serialize};

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
    /// 0-indexed line indices from raw output that the filter considers informative.
    pub kept_lines: Vec<usize>,
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
    Exact(Arc<str>),
    Prefix(Arc<str>),
    Regex(regex::Regex),
    #[cfg(test)]
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
            Self::Exact(s) => command == s.as_ref(),
            Self::Prefix(s) => command.starts_with(s.as_ref()),
            Self::Regex(re) => re.is_match(command),
            #[cfg(test)]
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
            #[cfg(test)]
            Self::Custom(_) => write!(f, "Custom(...)"),
        }
    }
}

// ---------------------------------------------------------------------------
// OutputFilter trait
// ---------------------------------------------------------------------------

/// Command-aware output filter.
pub trait OutputFilter: Send + Sync {
    fn name(&self) -> &str;
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
        let mut kept_lines: Vec<usize> = Vec::new();

        for stage in &self.stages {
            let result = stage.filter(command, &current, exit_code);
            worst = worse_confidence(worst, result.confidence);
            if !result.kept_lines.is_empty() {
                kept_lines.clone_from(&result.kept_lines);
            }
            current = result.output;
        }

        FilterResult {
            raw_chars: initial_len,
            filtered_chars: current.len(),
            raw_lines: count_lines(output),
            filtered_lines: count_lines(&current),
            output: current,
            confidence: worst,
            kept_lines,
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

pub(crate) fn default_true() -> bool {
    true
}

/// Configuration for output filters.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub security: SecurityFilterConfig,

    /// Directory containing a `filters.toml` override file.
    /// Falls back to embedded defaults when `None` or when the file is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters_path: Option<PathBuf>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            security: SecurityFilterConfig::default(),
            filters_path: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
        for f in declarative::load_declarative_filters(config.filters_path.as_deref()) {
            r.register(f);
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
        let mut m = self.metrics.lock();
        m.record(result);
        if m.total_commands.is_multiple_of(50) {
            tracing::debug!(
                total = m.total_commands,
                filtered = m.filtered_commands,
                savings_pct = format!("{:.1}", m.savings_pct()),
                "filter metrics"
            );
        }
    }

    #[must_use]
    pub fn metrics(&self) -> FilterMetrics {
        self.metrics.lock().clone()
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

fn make_result(
    raw: &str,
    output: String,
    confidence: FilterConfidence,
    kept_lines: Vec<usize>,
) -> FilterResult {
    let filtered_chars = output.len();
    FilterResult {
        raw_lines: count_lines(raw),
        filtered_lines: count_lines(&output),
        output,
        raw_chars: raw.len(),
        filtered_chars,
        confidence,
        kept_lines,
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
            kept_lines: vec![],
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
            kept_lines: vec![],
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
        let r = make_result(raw, filtered, FilterConfidence::Full, vec![]);
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
        assert!(c.security.enabled);
    }

    #[test]
    fn filter_config_deserialize_security() {
        let toml_str = r#"
enabled = true

[security]
enabled = true
extra_patterns = ["TODO: security review"]
"#;
        let c: FilterConfig = toml::from_str(toml_str).unwrap();
        assert!(c.enabled);
        assert_eq!(c.security.extra_patterns, vec!["TODO: security review"]);
    }

    // CommandMatcher tests
    #[test]
    fn command_matcher_exact() {
        let m = CommandMatcher::Exact(Arc::from("ls"));
        assert!(m.matches("ls"));
        assert!(!m.matches("ls -la"));
    }

    #[test]
    fn command_matcher_prefix() {
        let m = CommandMatcher::Prefix(Arc::from("git "));
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
        let m = CommandMatcher::Prefix(Arc::from("cargo "));
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
        let m = CommandMatcher::Exact(Arc::from("ls"));
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
            kept_lines: vec![],
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
            make_result(raw, output, self.confidence, vec![])
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

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn filter_pipeline_run_never_panics(cmd in ".*", output in ".*", exit_code in -1i32..=255) {
            let pipeline = FilterPipeline::new();
            let _ = pipeline.run(&cmd, &output, exit_code);
        }

        #[test]
        fn output_filter_registry_apply_never_panics(cmd in ".*", output in ".*", exit_code in -1i32..=255) {
            let reg = OutputFilterRegistry::new(true);
            let _ = reg.apply(&cmd, &output, exit_code);
        }
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
