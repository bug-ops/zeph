//! Declarative TOML-based output filter engine.
//!
//! Loads filter rules from a TOML file and compiles them into [`OutputFilter`]
//! implementations at startup. Supports `strip_noise` and `truncate` strategies.

use std::fmt::Write as _;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use super::{
    CommandMatcher, FilterConfidence, FilterResult, OutputFilter, make_result, sanitize_output,
};

// ---------------------------------------------------------------------------
// Deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct DeclarativeFilterFile {
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

#[derive(Deserialize)]
pub(crate) struct RuleConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    pub strategy: StrategyConfig,
    #[serde(default = "super::default_true")]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct MatchConfig {
    pub exact: Option<String>,
    pub prefix: Option<String>,
    pub regex: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StrategyConfig {
    StripNoise {
        patterns: Vec<String>,
    },
    Truncate {
        max_lines: usize,
        #[serde(default = "default_head")]
        head: usize,
        #[serde(default = "default_tail")]
        tail: usize,
    },
}

fn default_head() -> usize {
    20
}

fn default_tail() -> usize {
    20
}

// ---------------------------------------------------------------------------
// Compiled runtime types
// ---------------------------------------------------------------------------

pub(crate) enum CompiledStrategy {
    StripNoise {
        patterns: Vec<Regex>,
    },
    Truncate {
        max_lines: usize,
        head: usize,
        tail: usize,
    },
}

pub(crate) struct DeclarativeFilter {
    name: &'static str,
    matcher: CommandMatcher,
    strategy: CompiledStrategy,
}

impl DeclarativeFilter {
    pub fn compile(rule: RuleConfig) -> Result<Self, String> {
        let name: &'static str = Box::leak(rule.name.into_boxed_str());
        let matcher = compile_match(&rule.match_config)?;
        let strategy = compile_strategy(rule.strategy)?;
        Ok(Self {
            name,
            matcher,
            strategy,
        })
    }
}

fn compile_match(m: &MatchConfig) -> Result<CommandMatcher, String> {
    if let Some(ref exact) = m.exact {
        let s: &'static str = Box::leak(exact.clone().into_boxed_str());
        Ok(CommandMatcher::Exact(s))
    } else if let Some(ref prefix) = m.prefix {
        let s: &'static str = Box::leak(prefix.clone().into_boxed_str());
        Ok(CommandMatcher::Prefix(s))
    } else if let Some(ref regex) = m.regex {
        if regex.len() > 512 {
            return Err("regex pattern exceeds 512 character limit".into());
        }
        let re = RegexBuilder::new(regex)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| format!("invalid regex: {e}"))?;
        Ok(CommandMatcher::Regex(re))
    } else {
        Err("match config must have exactly one of: exact, prefix, regex".into())
    }
}

fn compile_strategy(s: StrategyConfig) -> Result<CompiledStrategy, String> {
    match s {
        StrategyConfig::StripNoise { patterns } => {
            let compiled = patterns
                .iter()
                .map(|p| {
                    if p.len() > 512 {
                        return Err(format!("pattern '{p}': exceeds 512 character limit"));
                    }
                    RegexBuilder::new(p)
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|e| format!("pattern '{p}': {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompiledStrategy::StripNoise { patterns: compiled })
        }
        StrategyConfig::Truncate {
            max_lines,
            head,
            tail,
        } => {
            if head + tail > max_lines {
                return Err("head + tail must not exceed max_lines".into());
            }
            Ok(CompiledStrategy::Truncate {
                max_lines,
                head,
                tail,
            })
        }
    }
}

impl OutputFilter for DeclarativeFilter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn matcher(&self) -> &CommandMatcher {
        &self.matcher
    }

    fn filter(&self, _command: &str, raw_output: &str, _exit_code: i32) -> FilterResult {
        let clean = sanitize_output(raw_output);
        match &self.strategy {
            CompiledStrategy::StripNoise { patterns } => {
                let filtered: String = clean
                    .lines()
                    .filter(|line| !patterns.iter().any(|p| p.is_match(line)))
                    .collect::<Vec<_>>()
                    .join("\n");
                if filtered.len() < clean.len() {
                    make_result(raw_output, filtered, FilterConfidence::Full)
                } else {
                    make_result(raw_output, clean, FilterConfidence::Fallback)
                }
            }
            CompiledStrategy::Truncate {
                max_lines,
                head,
                tail,
            } => {
                let lines: Vec<&str> = clean.lines().collect();
                if lines.len() <= *max_lines {
                    return make_result(raw_output, clean, FilterConfidence::Fallback);
                }
                let omitted = lines.len() - head - tail;
                let mut output = String::new();
                for line in &lines[..*head] {
                    output.push_str(line);
                    output.push('\n');
                }
                let _ = write!(output, "\n... ({omitted} lines omitted) ...\n\n");
                for line in &lines[lines.len() - tail..] {
                    output.push_str(line);
                    output.push('\n');
                }
                make_result(
                    raw_output,
                    output.trim_end().to_owned(),
                    FilterConfidence::Partial,
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load declarative filters from `config_dir/filters.toml`, falling back to
/// embedded defaults when the file is absent or `config_dir` is `None`.
pub(crate) fn load_declarative_filters(config_dir: Option<&Path>) -> Vec<Box<dyn OutputFilter>> {
    let file_content = if let Some(dir) = config_dir {
        let path = dir.join("filters.toml");
        let load_result = std::fs::metadata(&path)
            .map_err(|e| e.to_string())
            .and_then(|meta| {
                if meta.len() >= 1_048_576 {
                    Err(format!(
                        "filters.toml exceeds 1 MiB limit ({} bytes)",
                        meta.len()
                    ))
                } else {
                    std::fs::read_to_string(&path).map_err(|e| e.to_string())
                }
            });
        match load_result {
            Ok(content) => {
                tracing::debug!(path = %path.display(), "loaded user filters.toml");
                content
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "failed to load filters.toml: {e}");
                include_str!("default-filters.toml").to_owned()
            }
        }
    } else {
        include_str!("default-filters.toml").to_owned()
    };

    let parsed: DeclarativeFilterFile = match toml::from_str(&file_content) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("failed to parse filters.toml: {e}");
            return Vec::new();
        }
    };

    let mut filters: Vec<Box<dyn OutputFilter>> = Vec::new();
    for rule in parsed.rules {
        if !rule.enabled {
            continue;
        }
        let name = rule.name.clone();
        match DeclarativeFilter::compile(rule) {
            Ok(f) => filters.push(Box::new(f)),
            Err(e) => tracing::warn!("skipping rule '{name}': {e}"),
        }
    }
    filters
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_noise_filter(patterns: &[&str]) -> DeclarativeFilter {
        DeclarativeFilter {
            name: "test-strip",
            matcher: CommandMatcher::Prefix("cmd"),
            strategy: CompiledStrategy::StripNoise {
                patterns: patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
            },
        }
    }

    fn truncate_filter(max_lines: usize, head: usize, tail: usize) -> DeclarativeFilter {
        DeclarativeFilter {
            name: "test-truncate",
            matcher: CommandMatcher::Prefix("cmd"),
            strategy: CompiledStrategy::Truncate {
                max_lines,
                head,
                tail,
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
        assert!(matches!(matcher, CommandMatcher::Exact("ls")));
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
        // 20 total, head=2, tail=2 → 16 omitted
        assert!(result.output.contains("16 lines omitted"));
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
        // Write > 1 MiB
        let chunk = "# filler\n".repeat(120_000);
        std::fs::write(&path, chunk).unwrap();
        // Should fall back to defaults, not panic or return empty
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

        let mut config = FilterConfig::default();
        config.filters_path = Some(dir.path().to_path_buf());

        let registry = OutputFilterRegistry::default_filters(&config);
        let raw = "npm warn deprecated pkg\nnpm notice created tarball\nDone installing";
        let result = registry.apply("npm install lodash", raw, 0);
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(!out.output.contains("npm warn"));
        assert!(!out.output.contains("npm notice"));
        assert!(out.output.contains("Done installing"));
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
    }
}
